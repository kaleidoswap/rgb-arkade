package main

import (
	"bytes"
	"crypto/sha256"
	"encoding/binary"
	"testing"

	"github.com/arkade-os/emulator/pkg/arkade"
	"github.com/btcsuite/btcd/btcec/v2"
	"github.com/btcsuite/btcd/btcec/v2/schnorr"
	"github.com/btcsuite/btcd/chainhash/v2"
	"github.com/btcsuite/btcd/txscript/v2"
	"github.com/btcsuite/btcd/wire/v2"
	"github.com/stretchr/testify/require"
)

type fetcher struct{ *txscript.MultiPrevOutFetcher }

func (fetcher) FetchPrevOutArkTx(wire.OutPoint) *wire.MsgTx   { return nil }
func (fetcher) FetchVtxoPrevOutPkScript(wire.OutPoint) []byte { return nil }

func opret(c []byte) []byte { s, _ := txscript.NullDataScript(c); return s }

type fixture struct {
	script         []byte
	cp, ark        *wire.MsgTx
	sig, c1, c2    []byte
	makerSpk, p2tr []byte
	price          int64
	maker          *btcec.PrivateKey
}

func newFixture(t *testing.T) *fixture {
	maker, err := btcec.NewPrivateKey()
	require.NoError(t, err)
	makerSpk := append([]byte{0x51, 0x20}, schnorr.SerializePubKey(maker.PubKey())...)
	p2tr := append([]byte{0x51, 0x20}, bytes.Repeat([]byte{0x11}, 32)...)
	c1, c2 := bytes.Repeat([]byte{0xc1}, 32), bytes.Repeat([]byte{0xc2}, 32)
	const price = 30_000
	script, err := Covenant(schnorr.SerializePubKey(maker.PubKey()), makerSpk, price)
	require.NoError(t, err)
	v := wire.OutPoint{Hash: chainhash.Hash{7}, Index: 2}
	sig := signOffer(t, maker, c1, c2, v)

	cp := wire.NewMsgTx(3)
	cp.AddTxIn(&wire.TxIn{PreviousOutPoint: v, Sequence: 0xffffffff})
	cp.AddTxOut(&wire.TxOut{Value: 0, PkScript: opret(c1)})
	cp.AddTxOut(&wire.TxOut{Value: 2_000, PkScript: p2tr})
	cp.AddTxOut(&wire.TxOut{Value: 0, PkScript: []byte{0x51, 0x02, 0x4e, 0x73}})

	ark := wire.NewMsgTx(3)
	ark.AddTxIn(&wire.TxIn{PreviousOutPoint: wire.OutPoint{Hash: chainhash.Hash{9}, Index: 0}})
	ark.AddTxIn(&wire.TxIn{PreviousOutPoint: wire.OutPoint{Hash: cp.TxHash(), Index: 1}})
	ark.AddTxOut(&wire.TxOut{Value: 1, PkScript: opret(c2)})
	ark.AddTxOut(&wire.TxOut{Value: price + 2_000, PkScript: makerSpk})
	ark.AddTxOut(&wire.TxOut{Value: 1_000, PkScript: p2tr})
	return &fixture{script, cp, ark, sig, c1, c2, makerSpk, p2tr, price, maker}
}

// signOffer: the maker's BIP340 signature over SHA256(C1 || C2 || outpoint of V).
func signOffer(t *testing.T, k *btcec.PrivateKey, c1, c2 []byte, v wire.OutPoint) []byte {
	var idx [4]byte
	binary.LittleEndian.PutUint32(idx[:], v.Index)
	m := sha256.Sum256(append(append(append(append([]byte{}, c1...), c2...), v.Hash[:]...), idx[:]...))
	sig, err := schnorr.Sign(k, m[:])
	require.NoError(t, err)
	return sig.Serialize()
}

func (f *fixture) run(t *testing.T) error {
	var raw bytes.Buffer
	require.NoError(t, f.cp.SerializeNoWitness(&raw))
	pf := txscript.NewMultiPrevOutFetcher(nil)
	pf.AddPrevOut(f.ark.TxIn[0].PreviousOutPoint, &wire.TxOut{Value: 100_000, PkScript: f.p2tr})
	pf.AddPrevOut(f.ark.TxIn[1].PreviousOutPoint, f.cp.TxOut[1])
	e, err := arkade.NewEngine(f.script, f.ark, 1, txscript.NewSigCache(10), txscript.NewTxSigHashes(f.ark, pf), 2_000, fetcher{pf})
	require.NoError(t, err)
	e.SetStack([][]byte{f.sig, f.c1, f.c2, raw.Bytes()})
	return e.Execute()
}

func TestCovenant(t *testing.T) {
	t.Run("valid fill", func(t *testing.T) {
		require.NoError(t, newFixture(t).run(t))
	})
	t.Run("exact price passes, price-1 fails", func(t *testing.T) {
		f := newFixture(t)
		f.ark.TxOut[1].Value = f.price
		require.NoError(t, f.run(t))
		f.ark.TxOut[1].Value = f.price - 1
		require.Error(t, f.run(t))
	})
	t.Run("maker output redirected", func(t *testing.T) {
		f := newFixture(t)
		f.ark.TxOut[1].PkScript = f.p2tr
		require.Error(t, f.run(t))
	})
	t.Run("other T2 commitment at output 0", func(t *testing.T) {
		f := newFixture(t)
		f.ark.TxOut[0].PkScript = opret(bytes.Repeat([]byte{0xdd}, 32))
		require.Error(t, f.run(t))
	})
	t.Run("no commitment at output 0", func(t *testing.T) {
		f := newFixture(t)
		f.ark.TxOut[0] = &wire.TxOut{Value: 1_000, PkScript: f.p2tr}
		require.Error(t, f.run(t))
	})
	t.Run("checkpoint with another T1", func(t *testing.T) {
		f := newFixture(t)
		f.cp.TxOut[0].PkScript = opret(bytes.Repeat([]byte{0xee}, 32))
		f.ark.TxIn[1].PreviousOutPoint.Hash = f.cp.TxHash()
		require.Error(t, f.run(t))
	})
	t.Run("witness claims C1 that the checkpoint does not carry", func(t *testing.T) {
		f := newFixture(t)
		f.c1 = bytes.Repeat([]byte{0xee}, 32)
		require.Error(t, f.run(t))
	})
	t.Run("raw checkpoint not the spent one", func(t *testing.T) {
		f := newFixture(t)
		f.ark.TxIn[1].PreviousOutPoint.Hash = chainhash.Hash{1}
		require.Error(t, f.run(t))
	})
	t.Run("spending checkpoint vout 0", func(t *testing.T) {
		f := newFixture(t)
		f.ark.TxIn[1].PreviousOutPoint.Index = 0
		require.Error(t, f.run(t))
	})
	t.Run("C1/C2 not signed by the maker", func(t *testing.T) {
		f := newFixture(t)
		other, _ := btcec.NewPrivateKey()
		f.sig = signOffer(t, other, f.c1, f.c2, f.cp.TxIn[0].PreviousOutPoint)
		require.Error(t, f.run(t))
	})
	t.Run("signature replayed on another VTXO at the same address", func(t *testing.T) {
		f := newFixture(t) // sig is for V = (7..., 2)
		f.cp.TxIn[0].PreviousOutPoint = wire.OutPoint{Hash: chainhash.Hash{8}, Index: 0}
		f.ark.TxIn[1].PreviousOutPoint.Hash = f.cp.TxHash()
		require.Error(t, f.run(t))
		f.sig = signOffer(t, f.maker, f.c1, f.c2, f.cp.TxIn[0].PreviousOutPoint)
		require.NoError(t, f.run(t)) // the same tx with a signature for that VTXO passes
	})
	t.Run("maker-signed pair, swapped order", func(t *testing.T) {
		f := newFixture(t)
		f.c1, f.c2 = f.c2, f.c1
		f.cp.TxOut[0].PkScript = opret(f.c1)
		f.ark.TxIn[1].PreviousOutPoint.Hash = f.cp.TxHash()
		f.ark.TxOut[0].PkScript = opret(f.c2)
		require.Error(t, f.run(t))
	})
}
