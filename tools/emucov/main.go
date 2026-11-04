package main

import (
	"encoding/hex"
	"fmt"
	"os"
	"strconv"
	"strings"

	"github.com/arkade-os/arkd/pkg/ark-lib/extension"
	"github.com/arkade-os/emulator/pkg/arkade"
	"github.com/btcsuite/btcd/btcec/v2/schnorr"
	"github.com/btcsuite/btcd/wire/v2"
)

func must(err error) {
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}

func unhex(s string) []byte {
	b, err := hex.DecodeString(s)
	must(err)
	return b
}

// usage:
//
//	emucov script <maker_xonly> <maker_spk> <price_sat>      -> covenant script hex
//	emucov tweak <emulator_pubkey> <script>                   -> tweaked x-only key hex
//	emucov outcome-script <merkle_root> [<asset_txid> <gidx> <min>] -> outcome covenant script hex
//	emucov tree <leaf_hex>...                                 -> "<root> <proof_0> <proof_1> ..." (empty proof = "")
//	emucov ext <vin> <script> <witness_item>[,<witness_item>] [<type>:<hex>...] -> extension OP_RETURN pkScript hex
func main() {
	if len(os.Args) < 2 {
		must(fmt.Errorf("usage: emucov script|tweak|ext ..."))
	}
	switch os.Args[1] {
	case "script":
		price, err := strconv.ParseInt(os.Args[4], 10, 64)
		must(err)
		s, err := Covenant(unhex(os.Args[2]), unhex(os.Args[3]), price)
		must(err)
		fmt.Println(hex.EncodeToString(s))
	case "tweak":
		raw := unhex(os.Args[2])
		if len(raw) == 33 {
			raw = raw[1:]
		}
		pk, err := schnorr.ParsePubKey(raw)
		must(err)
		t := arkade.ComputeArkadeScriptPublicKey(pk, arkade.ArkadeScriptHash(unhex(os.Args[3])))
		fmt.Println(hex.EncodeToString(schnorr.SerializePubKey(t)))
	case "ext":
		vin, err := strconv.ParseUint(os.Args[2], 10, 16)
		must(err)
		var wit wire.TxWitness
		for _, w := range strings.Split(os.Args[4], ",") {
			wit = append(wit, unhex(w))
		}
		p, err := arkade.NewPacket(arkade.EmulatorEntry{Vin: uint16(vin), Script: unhex(os.Args[3]), Witness: wit})
		must(err)
		ext := extension.Extension{}
		// extra packets: <type>:<hex payload> (e.g. the Arkade asset packet, type 0)
		for _, extra := range os.Args[5:] {
			t, data, ok := strings.Cut(extra, ":")
			if !ok {
				must(fmt.Errorf("extra packet %q: want <type>:<hex>", extra))
			}
			ty, err := strconv.ParseUint(t, 10, 8)
			must(err)
			full, err := extension.NewExtensionFromPackets(extension.UnknownPacket{PacketType: uint8(ty), Data: unhex(data)})
			must(err)
			ext = append(ext, full...)
		}
		ext = append(ext, p)
		out, err := ext.TxOut()
		must(err)
		fmt.Println(hex.EncodeToString(out.PkScript))
	case "outcome-script":
		var aTxid []byte
		var gidx, amin uint64
		if len(os.Args) >= 6 {
			aTxid = unhex(os.Args[3])
			var err error
			gidx, err = strconv.ParseUint(os.Args[4], 10, 16)
			must(err)
			amin, err = strconv.ParseUint(os.Args[5], 10, 64)
			must(err)
		}
		s, err := OutcomeCovenantAsset(unhex(os.Args[2]), aTxid, uint16(gidx), amin)
		must(err)
		fmt.Println(hex.EncodeToString(s))
	case "tree":
		var leaves [][]byte
		for _, l := range os.Args[2:] {
			leaves = append(leaves, unhex(l))
		}
		root, proofs := MerkleTree(leaves)
		out := []string{hex.EncodeToString(root)}
		for _, p := range proofs {
			out = append(out, hex.EncodeToString(p))
		}
		fmt.Println(strings.Join(out, " "))
	default:
		must(fmt.Errorf("unknown command %s", os.Args[1]))
	}
}
