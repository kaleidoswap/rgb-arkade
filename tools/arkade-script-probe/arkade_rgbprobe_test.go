package arkade

// [rgb-arkade probe] Can Arkade Script see/enforce an RGB opret commitment?

import (
	"bytes"
	"crypto/sha256"
	"testing"

	"github.com/btcsuite/btcd/chainhash/v2"
	"github.com/btcsuite/btcd/txscript/v2"
	"github.com/btcsuite/btcd/wire/v2"
	"github.com/stretchr/testify/require"
)

func probeRun(t *testing.T, script []byte, tx *wire.MsgTx, prev map[wire.OutPoint]*wire.TxOut, stack [][]byte) error {
	t.Helper()
	pf := txscript.NewMultiPrevOutFetcher(nil)
	for op, o := range prev {
		pf.AddPrevOut(op, o)
	}
	f := newTestArkPrevOutFetcher(pf, nil, nil)
	e, err := NewEngine(script, tx, 0, txscript.NewSigCache(10), txscript.NewTxSigHashes(tx, pf), 0, f)
	require.NoError(t, err)
	e.SetStack(stack)
	return e.Execute()
}

func opret(c []byte) []byte { s, _ := txscript.NullDataScript(c); return s }

func TestRGBProbeEngine(t *testing.T) {
	C := bytes.Repeat([]byte{0xAB}, 32)
	D := bytes.Repeat([]byte{0xCD}, 32)
	p2tr := append([]byte{0x51, 0x20}, bytes.Repeat([]byte{0x11}, 32)...)

	// checkpoint (our layout): [opret C, cp output, anchor]
	cp := wire.NewMsgTx(3)
	cp.AddTxIn(&wire.TxIn{PreviousOutPoint: wire.OutPoint{Hash: chainhash.Hash{7}}, Sequence: 0xffffffff})
	cp.AddTxOut(&wire.TxOut{Value: 0, PkScript: opret(C)})
	cp.AddTxOut(&wire.TxOut{Value: 1000, PkScript: p2tr})
	cp.AddTxOut(&wire.TxOut{Value: 0, PkScript: []byte{0x51, 0x02, 0x4e, 0x73}})
	var raw bytes.Buffer
	require.NoError(t, cp.SerializeNoWitness(&raw))

	mkArk := func(out0 []byte, spendVout uint32) (*wire.MsgTx, map[wire.OutPoint]*wire.TxOut) {
		tx := wire.NewMsgTx(3)
		op := wire.OutPoint{Hash: cp.TxHash(), Index: spendVout}
		tx.AddTxIn(&wire.TxIn{PreviousOutPoint: op})
		tx.AddTxOut(&wire.TxOut{Value: 0, PkScript: out0})
		tx.AddTxOut(&wire.TxOut{Value: 1000, PkScript: p2tr})
		return tx, map[wire.OutPoint]*wire.TxOut{op: {Value: 1000, PkScript: p2tr}}
	}

	// (a) pinned commitment: output 0 of THIS tx must be OP_RETURN <C>
	h := sha256.Sum256(opret(C))
	sa, _ := txscript.NewScriptBuilder().AddInt64(0).AddOp(OP_INSPECTOUTPUTSCRIPTPUBKEY).
		AddInt64(-1).AddOp(OP_EQUALVERIFY).AddData(h[:]).AddOp(OP_EQUAL).Script()
	tx, pv := mkArk(opret(C), 1)
	t.Logf("(a) pinned opret C, tx has C      -> %v", probeRun(t, sa, tx, pv, nil))
	tx, pv = mkArk(opret(D), 1)
	t.Logf("(a) pinned opret C, tx has D      -> %v", probeRun(t, sa, tx, pv, nil))

	// (b) "some 32-byte opret at output 0" (anti-burn): witness supplies the 32 bytes
	sb, _ := txscript.NewScriptBuilder().AddOp(OP_SIZE).AddInt64(32).AddOp(OP_EQUALVERIFY).
		AddData([]byte{0x6a, 0x20}).AddOp(OP_SWAP).AddOp(OP_CAT).AddOp(OP_SHA256).
		AddInt64(0).AddOp(OP_INSPECTOUTPUTSCRIPTPUBKEY).AddInt64(-1).AddOp(OP_EQUALVERIFY).
		AddOp(OP_EQUAL).Script()
	tx, pv = mkArk(opret(D), 1)
	t.Logf("(b) any opret, tx has D, wit D    -> %v", probeRun(t, sb, tx, pv, [][]byte{D}))
	tx, pv = mkArk(p2tr, 1)
	t.Logf("(b) any opret, tx has none, wit D -> %v", probeRun(t, sb, tx, pv, [][]byte{D}))

	// (c) from the ark tx, check the CHECKPOINT's opret (the emulator never runs scripts on checkpoints):
	// witness = raw checkpoint tx; HASH256 == spent txid, spent vout == 1, bytes[56:90] == 6a20||C
	sc, _ := txscript.NewScriptBuilder().
		AddOp(OP_DUP).AddOp(OP_HASH256).
		AddInt64(0).AddOp(OP_INSPECTINPUTOUTPOINT).AddInt64(1).AddOp(OP_EQUALVERIFY).
		AddOp(OP_EQUALVERIFY).
		AddInt64(56).AddInt64(34).AddOp(OP_SUBSTR).AddData(opret(C)).AddOp(OP_EQUAL).Script()
	tx, pv = mkArk(p2tr, 1)
	t.Logf("(c) checkpoint opret C via preimage -> %v", probeRun(t, sc, tx, pv, [][]byte{raw.Bytes()}))
	bad := append([]byte(nil), raw.Bytes()...)
	bad[60] ^= 1
	t.Logf("(c) tampered checkpoint bytes       -> %v", probeRun(t, sc, tx, pv, [][]byte{bad}))
}
