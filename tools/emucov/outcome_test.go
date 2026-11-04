package main

import (
	"bytes"
	"crypto/sha256"
	"encoding/binary"
	"testing"
	"time"

	"github.com/arkade-os/emulator/pkg/arkade"
	"github.com/btcsuite/btcd/btcec/v2"
	"github.com/btcsuite/btcd/btcec/v2/schnorr"
	"github.com/btcsuite/btcd/chainhash/v2"
	"github.com/btcsuite/btcd/txscript/v2"
	"github.com/btcsuite/btcd/wire/v2"
	"github.com/stretchr/testify/require"
)

func k32(b byte) (k [32]byte) { k[0] = b; k[31] = b; return }
func p2tr(k [32]byte) []byte  { return append([]byte{0x51, 0x20}, k[:]...) }
func c32(b byte) (c [32]byte) { copy(c[:], bytes.Repeat([]byte{b}, 32)); return }

type ofx struct {
	leaves  []Outcome
	idx     int
	script  []byte
	cp, ark *wire.MsgTx
	stmt    []byte
	authSig []byte
	leaf    []byte
	proof   []byte
	rawOver []byte
}

// newOfx: covenant over `leaves`; the tx satisfies leaves[idx] (outputs built from its fields).
func newOfx(t *testing.T, leaves []Outcome, idx int) *ofx {
	var raws [][]byte
	for _, l := range leaves {
		raws = append(raws, l.Bytes())
	}
	root, proofs := MerkleTree(raws)
	script, err := OutcomeCovenant(root)
	require.NoError(t, err)
	o := leaves[idx]
	v := wire.OutPoint{Hash: chainhash.Hash{7}, Index: 1}
	cp := wire.NewMsgTx(3)
	cp.AddTxIn(&wire.TxIn{PreviousOutPoint: v, Sequence: 0xffffffff})
	cp.AddTxOut(&wire.TxOut{Value: 0, PkScript: opret(o.C1[:])})
	cp.AddTxOut(&wire.TxOut{Value: 2_000, PkScript: p2tr(k32(0xcc))})
	cp.AddTxOut(&wire.TxOut{Value: 0, PkScript: []byte{0x51, 0x02, 0x4e, 0x73}})
	ark := wire.NewMsgTx(3)
	ark.AddTxIn(&wire.TxIn{PreviousOutPoint: wire.OutPoint{Hash: chainhash.Hash{9}, Index: 0}})
	ark.AddTxIn(&wire.TxIn{PreviousOutPoint: wire.OutPoint{Hash: cp.TxHash(), Index: 1}})
	ark.AddTxOut(&wire.TxOut{Value: 1, PkScript: opret(o.C2[:])})
	pay, rgb, next := k32(0xee), k32(0xef), k32(0xed)
	if o.PayKey != [32]byte{} {
		pay = o.PayKey
	}
	if o.RgbKey != [32]byte{} {
		rgb = o.RgbKey
	}
	if o.NextKey != [32]byte{} {
		next = o.NextKey
	}
	ark.AddTxOut(&wire.TxOut{Value: int64(o.PayMin) + 10, PkScript: p2tr(pay)})
	ark.AddTxOut(&wire.TxOut{Value: 1_000, PkScript: p2tr(rgb)})
	ark.AddTxOut(&wire.TxOut{Value: int64(o.NextMin) + 1, PkScript: p2tr(next)})
	return &ofx{leaves: leaves, idx: idx, script: script, cp: cp, ark: ark, leaf: raws[idx], proof: proofs[idx]}
}

func (f *ofx) sign(t *testing.T, k *btcec.PrivateKey) {
	var idx [4]byte
	binary.LittleEndian.PutUint32(idx[:], f.cp.TxIn[0].PreviousOutPoint.Index)
	m := sha256.Sum256(append(append(append([]byte{}, f.leaf...), f.cp.TxIn[0].PreviousOutPoint.Hash[:]...), idx[:]...))
	sig, err := schnorr.Sign(k, m[:])
	require.NoError(t, err)
	f.authSig = sig.Serialize()
}

func (f *ofx) oracle(t *testing.T, k *btcec.PrivateKey, price, ts uint64) {
	f.stmt = make([]byte, 16)
	binary.LittleEndian.PutUint64(f.stmt, price)
	binary.LittleEndian.PutUint64(f.stmt[8:], ts)
	m := sha256.Sum256(f.stmt)
	sig, err := schnorr.Sign(k, m[:])
	require.NoError(t, err)
	f.authSig = sig.Serialize()
}

func (f *ofx) run(t *testing.T) error {
	var raw bytes.Buffer
	require.NoError(t, f.cp.SerializeNoWitness(&raw))
	if f.rawOver != nil {
		raw.Reset()
		raw.Write(f.rawOver)
	}
	pf := txscript.NewMultiPrevOutFetcher(nil)
	pf.AddPrevOut(f.ark.TxIn[0].PreviousOutPoint, &wire.TxOut{Value: 100_000, PkScript: p2tr(k32(0xaa))})
	pf.AddPrevOut(f.ark.TxIn[1].PreviousOutPoint, f.cp.TxOut[1])
	e, err := arkade.NewEngine(f.script, f.ark, 1, txscript.NewSigCache(10), txscript.NewTxSigHashes(f.ark, pf), 2_000, fetcher{pf})
	require.NoError(t, err)
	e.SetStack([][]byte{f.stmt, f.authSig, f.proof, f.leaf, raw.Bytes()})
	return e.Execute()
}

func xonly(k *btcec.PrivateKey) (x [32]byte) { copy(x[:], schnorr.SerializePubKey(k.PubKey())); return }

func TestOutcomeCovenant(t *testing.T) {
	now := uint64(time.Now().Unix())
	maker, arb, orc, other := mustKey(t), mustKey(t), mustKey(t), mustKey(t)
	lots := []Outcome{
		{C1: c32(1), C2: c32(2), PayKey: xonly(maker), PayMin: 30_000, NextKey: k32(0x33), NextMin: 2_000, NotAfter: now + 600},
		{C1: c32(1), C2: c32(3), RgbKey: xonly(maker), NotBefore: now + 600},
		{C1: c32(1), C2: c32(4), RgbKey: k32(0x44), AuthKey: xonly(arb)},
		{C1: c32(1), C2: c32(5), RgbKey: k32(0x55), AuthKey: xonly(orc), PriceBelow: 50_000},
		{C1: c32(1), C2: c32(6), RgbKey: k32(0x66), NotBefore: now - 10},
	}
	t.Run("script size", func(t *testing.T) {
		f := newOfx(t, lots, 0)
		t.Logf("outcome covenant %d bytes, leaf %d bytes, proof %d bytes (5 leaves)", len(f.script), len(f.leaf), len(f.proof))
	})
	t.Run("lot fill before expiry", func(t *testing.T) { require.NoError(t, newOfx(t, lots, 0).run(t)) })
	t.Run("lot underpaid", func(t *testing.T) {
		f := newOfx(t, lots, 0)
		f.ark.TxOut[1].Value = 29_999
		{
			err := f.run(t)
			t.Logf("refused: %v", err)
			require.Error(t, err)
		}
	})
	t.Run("lot remainder redirected", func(t *testing.T) {
		f := newOfx(t, lots, 0)
		f.ark.TxOut[3].PkScript = p2tr(k32(0x99))
		{
			err := f.run(t)
			t.Logf("refused: %v", err)
			require.Error(t, err)
		}
	})
	t.Run("lot remainder value drained", func(t *testing.T) {
		f := newOfx(t, lots, 0)
		f.ark.TxOut[3].Value = 1_999
		{
			err := f.run(t)
			t.Logf("refused: %v", err)
			require.Error(t, err)
		}
	})
	t.Run("lot after expiry", func(t *testing.T) {
		l := append([]Outcome{}, lots...)
		l[0].NotAfter = now - 1
		{
			err := newOfx(t, l, 0).run(t)
			t.Logf("refused: %v", err)
			require.Error(t, err)
		}
	})
	t.Run("reclaim before expiry", func(t *testing.T) { require.Error(t, newOfx(t, lots, 1).run(t)) })
	t.Run("reclaim after expiry", func(t *testing.T) { require.NoError(t, newOfx(t, lots, 4).run(t)) })
	t.Run("reclaim to someone else", func(t *testing.T) {
		f := newOfx(t, lots, 4)
		f.ark.TxOut[2].PkScript = p2tr(k32(0x98))
		{
			err := f.run(t)
			t.Logf("refused: %v", err)
			require.Error(t, err)
		}
	})
	t.Run("leaf not in tree", func(t *testing.T) {
		f := newOfx(t, lots, 4)
		o := lots[4]
		o.RgbKey = k32(0x98)
		f.leaf = o.Bytes()
		f.ark.TxOut[2].PkScript = p2tr(o.RgbKey)
		{
			err := f.run(t)
			t.Logf("refused: %v", err)
			require.Error(t, err)
		}
	})
	t.Run("wrong C2 at output 0", func(t *testing.T) {
		f := newOfx(t, lots, 4)
		f.ark.TxOut[0].PkScript = opret(bytes.Repeat([]byte{0xdd}, 32))
		{
			err := f.run(t)
			t.Logf("refused: %v", err)
			require.Error(t, err)
		}
	})
	t.Run("checkpoint with another C1", func(t *testing.T) {
		f := newOfx(t, lots, 4)
		f.cp.TxOut[0].PkScript = opret(bytes.Repeat([]byte{0xee}, 32))
		f.ark.TxIn[1].PreviousOutPoint.Hash = f.cp.TxHash()
		{
			err := f.run(t)
			t.Logf("refused: %v", err)
			require.Error(t, err)
		}
	})
	t.Run("arbiter signed", func(t *testing.T) {
		f := newOfx(t, lots, 2)
		f.sign(t, arb)
		require.NoError(t, f.run(t))
	})
	t.Run("arbiter missing / wrong signer", func(t *testing.T) {
		f := newOfx(t, lots, 2)
		{
			err := f.run(t)
			t.Logf("refused: %v", err)
			require.Error(t, err)
		}
		f.sign(t, other)
		{
			err := f.run(t)
			t.Logf("refused: %v", err)
			require.Error(t, err)
		}
	})
	t.Run("arbiter signature replayed on another VTXO", func(t *testing.T) {
		f := newOfx(t, lots, 2)
		f.sign(t, arb)
		f.cp.TxIn[0].PreviousOutPoint.Index = 5
		f.ark.TxIn[1].PreviousOutPoint.Hash = f.cp.TxHash()
		{
			err := f.run(t)
			t.Logf("refused: %v", err)
			require.Error(t, err)
		}
	})
	t.Run("oracle price below threshold", func(t *testing.T) {
		f := newOfx(t, lots, 3)
		f.oracle(t, orc, 49_999, now-60)
		require.NoError(t, f.run(t))
	})
	t.Run("oracle price at threshold", func(t *testing.T) {
		f := newOfx(t, lots, 3)
		f.oracle(t, orc, 50_000, now-60)
		{
			err := f.run(t)
			t.Logf("refused: %v", err)
			require.Error(t, err)
		}
	})
	t.Run("oracle statement stale", func(t *testing.T) {
		f := newOfx(t, lots, 3)
		f.oracle(t, orc, 1, now-OracleMaxAge-5)
		{
			err := f.run(t)
			t.Logf("refused: %v", err)
			require.Error(t, err)
		}
	})
	t.Run("oracle statement forged", func(t *testing.T) {
		f := newOfx(t, lots, 3)
		f.oracle(t, other, 1, now-60)
		{
			err := f.run(t)
			t.Logf("refused: %v", err)
			require.Error(t, err)
		}
		f.oracle(t, orc, 60_000, now-60)
		binary.LittleEndian.PutUint64(f.stmt, 1) // edit the price after signing
		{
			err := f.run(t)
			t.Logf("refused: %v", err)
			require.Error(t, err)
		}
	})
	t.Run("single-leaf tree", func(t *testing.T) { require.NoError(t, newOfx(t, lots[4:], 0).run(t)) })
	t.Run("three-leaf tree, each leaf", func(t *testing.T) {
		l := []Outcome{lots[4], lots[4], lots[4]}
		l[1].C2, l[2].C2 = c32(8), c32(9)
		for i := range l {
			require.NoError(t, newOfx(t, l, i).run(t), "leaf %d", i)
		}
	})
}

func mustKey(t *testing.T) *btcec.PrivateKey {
	k, err := btcec.NewPrivateKey()
	require.NoError(t, err)
	return k
}
