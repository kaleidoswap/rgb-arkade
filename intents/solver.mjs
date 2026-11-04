// Demo Arkade Intents solver for RGB assets.
//
// Runs a minimal NIP-01 relay and a solver on it. The solver advertises a card (kind 38859) for
// `arkade:RGB:*->arkade:BTC`, answers sealed RFQs (kind 24859, NIP-44) with quotes, and reports
// settlement on `rfq_status_request`. Settlement is the one-tx atomic swap of the demo app
// (`/api/action` `swap`): the solver is wallet `bob` there, the buyer.
//
//   node intents/solver.mjs            # relay on ws://127.0.0.1:7447, app on http://127.0.0.1:8080
//
// Env: SOLVER_RELAY_PORT, SOLVER_APP_URL, SOLVER_PRICE (sat per unit, default 100),
// SOLVER_KEY_FILE (default state/intents-solver.key).

import { readFileSync, writeFileSync, mkdirSync } from "node:fs";
import { dirname } from "node:path";
import { WebSocketServer } from "ws";
import { finalizeEvent, generateSecretKey, getPublicKey, verifyEvent } from "nostr-tools/pure";
import { matchFilters } from "nostr-tools/filter";
import * as nip44 from "nostr-tools/nip44";
import { bytesToHex, hexToBytes } from "nostr-tools/utils";

const PORT = Number(process.env.SOLVER_RELAY_PORT ?? 7447);
const APP = process.env.SOLVER_APP_URL ?? "http://127.0.0.1:8080";
const PRICE = Number(process.env.SOLVER_PRICE ?? 100);
const KEY_FILE = process.env.SOLVER_KEY_FILE ?? new URL("../state/intents-solver.key", import.meta.url).pathname;
const RFQ_AD_KIND = 38859;
const RFQ_DIRECTED_KIND = 24859;
const BUYER = "bob";
const QUOTE_TTL = 120;

let secret;
try {
  secret = hexToBytes(readFileSync(KEY_FILE, "utf8").trim());
} catch {
  secret = generateSecretKey();
  mkdirSync(dirname(KEY_FILE), { recursive: true });
  writeFileSync(KEY_FILE, bytesToHex(secret));
}
const pubkey = getPublicKey(secret);
const log = (...a) => console.log(new Date().toISOString().slice(11, 19), ...a);

// ------------------------------------------------------------------------------------- relay

const stored = new Map(); // replaceable events: `${kind}:${pubkey}:${d}` -> event
const subs = new Map(); // socket -> Map<subId, filters>

function deliver(event) {
  for (const [socket, m] of subs) {
    for (const [id, filters] of m) {
      if (matchFilters(filters, event)) socket.send(JSON.stringify(["EVENT", id, event]));
    }
  }
}

function publish(event) {
  if (event.kind >= 30000 && event.kind < 40000) {
    const d = event.tags.find((t) => t[0] === "d")?.[1] ?? "";
    stored.set(`${event.kind}:${event.pubkey}:${d}`, event);
  }
  deliver(event);
  if (event.kind === RFQ_DIRECTED_KIND && event.tags.some((t) => t[0] === "p" && t[1] === pubkey)) {
    onRfq(event).catch((e) => log("rfq error", e.message));
  }
}

// loopback only, and no web page: local tools send no Origin, the wallet a browser-extension one
const wss = new WebSocketServer({
  host: "127.0.0.1",
  port: PORT,
  verifyClient: ({ origin }) => origin === undefined || /^(chrome|moz)-extension:\/\/[a-z0-9-]+$/.test(origin),
});
wss.on("connection", (socket) => {
  subs.set(socket, new Map());
  socket.on("message", (raw) => {
    try {
      const frame = JSON.parse(raw.toString());
      if (!Array.isArray(frame)) return;
      const [verb, a, ...rest] = frame;
      if (verb === "REQ" && typeof a === "string") {
        subs.get(socket).set(a, rest);
        for (const e of stored.values()) if (matchFilters(rest, e)) socket.send(JSON.stringify(["EVENT", a, e]));
        socket.send(JSON.stringify(["EOSE", a]));
      } else if (verb === "CLOSE") {
        subs.get(socket).delete(a);
      } else if (verb === "EVENT") {
        const ok = a && typeof a === "object" && Array.isArray(a.tags) && verifyEvent(a);
        socket.send(JSON.stringify(["OK", a?.id ?? "", !!ok, ok ? "" : "invalid: bad event"]));
        if (ok) publish(a);
      }
    } catch (e) {
      log("relay: dropped frame", e.message);
    }
  });
  socket.on("close", () => subs.delete(socket));
});

// ------------------------------------------------------------------------------------ solver

async function app(path, body) {
  const r = await fetch(APP + path, body ? { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body) } : {});
  return r.json();
}

function reply(to, payload) {
  const key = nip44.v2.utils.getConversationKey(secret, to);
  publish(
    finalizeEvent(
      { kind: RFQ_DIRECTED_KIND, created_at: Math.floor(Date.now() / 1000), tags: [["p", to]], content: nip44.v2.encrypt(JSON.stringify(payload), key) },
      secret,
    ),
  );
}

function card() {
  return finalizeEvent(
    {
      kind: RFQ_AD_KIND,
      created_at: Math.floor(Date.now() / 1000),
      tags: [["d", "rgb-arkade-demo"], ["pair", "arkade:RGB:*->arkade:BTC"]],
      content: JSON.stringify({
        v: 1,
        name: "KaleidoSwap demo solver",
        pairs: [{ pair: "arkade:RGB:*->arkade:BTC", price_sat_per_unit: PRICE, min_units: 1, settlement: "rgb-arkade-swap/1" }],
      }),
    },
    secret,
  );
}

const quotes = new Map(); // `${requester pubkey}:${rfq_id}` -> quote

async function onRfq(event) {
  const key = nip44.v2.utils.getConversationKey(secret, event.pubkey);
  let req;
  try {
    req = JSON.parse(nip44.v2.decrypt(event.content, key));
  } catch {
    return;
  }
  const refuse = (reason) => reply(event.pubkey, { v: 1, type: "rfq_refusal", rfq_id: req.rfq_id, reason });
  if (req.type === "rfq_status_request") {
    const q = quotes.get(`${event.pubkey}:${req.rfq_id}`);
    if (!q) return refuse("unknown_rfq");
    return reply(event.pubkey, { v: 1, type: "rfq_status", rfq_id: req.rfq_id, ...(await settlement(q)) });
  }
  if (req.type !== "rfq_request") return;
  const m = /^arkade:RGB:(.+)->arkade:BTC$/.exec(req.pair ?? "");
  const units = Number(req.from_amount);
  if (!m || !Number.isInteger(units) || units <= 0) return refuse("pair_not_supported");
  if (typeof req.rfq_id !== "string" || quotes.has(`${event.pubkey}:${req.rfq_id}`)) return refuse("duplicate_rfq");
  const sats = units * PRICE;
  const state = await app("/api/state");
  const bob = state.wallets?.find((w) => w.name === BUYER);
  const free = bob?.btc ? bob.btc.offchain_confirmed + bob.btc.offchain_preconfirmed : 0;
  if (free < sats + 1000) return refuse("insufficient_liquidity");
  const quote = {
    v: 1,
    type: "rfq_quote",
    rfq_id: req.rfq_id,
    pair: req.pair,
    from_amount: units,
    to_amount: sats,
    solver_pubkey: pubkey,
    valid_until: Math.floor(Date.now() / 1000) + QUOTE_TTL,
    profile: { asset_id: m[1], buyer: BUYER, settlement: "rgb-arkade-swap/1" },
  };
  quotes.set(`${event.pubkey}:${req.rfq_id}`, { ...quote, label: `-> ${BUYER}: swap ${units} for ${sats} sat`, since: Date.now() / 1000 - 5 });
  log(`quote ${req.rfq_id}: ${units} units -> ${sats} sat`);
  reply(event.pubkey, quote);
}

// The swap's progress, read from the app log: entries of the action whose label matches the quote.
async function settlement(q) {
  const s = await app("/api/state");
  const mine = (s.log ?? []).filter((e) => e.action.endsWith(q.label) && e.t >= q.since);
  const ark = mine.find((e) => e.kind === "tx" && /Arkade tx/.test(e.msg))?.msg.split(": ").pop();
  if (mine.some((e) => e.kind === "error")) return { state: "failed", detail: mine.findLast((e) => e.kind === "error").msg };
  if (mine.some((e) => e.kind === "ok")) return { state: "settled", txid: ark };
  if (mine.length) return { state: "settling", txid: ark };
  return { state: Date.now() / 1000 > q.valid_until ? "expired" : "quoted" };
}

publish(card());
setInterval(() => publish(card()), 600_000);
log(`relay ws://127.0.0.1:${PORT}, solver ${pubkey}, ${PRICE} sat/unit, app ${APP}`);
