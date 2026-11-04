// Package main: emucov, helper for the RGB fixed-price-sale covenant (scenario swap-emu).
package main

import (
	"github.com/arkade-os/emulator/pkg/arkade"
	"github.com/btcsuite/btcd/txscript/v2"
)

// CheckpointSpkOffset is where output 0's scriptPubKey starts in a serialized (no-witness)
// checkpoint tx with 1 input and an empty scriptSig: version 4 + vin count 1 + outpoint 36 +
// scriptSig len 1 + sequence 4 + vout count 1 + value 8 + spk len 1.
const CheckpointSpkOffset = 56

// CheckpointPrevoutOffset: the checkpoint's input 0 outpoint (txid 32 + vout 4) after version 4 + vin count 1.
const CheckpointPrevoutOffset = 5

// Covenant builds the fill leaf's Arkade Script of a fixed-price RGB sale.
//
// Witness (bottom -> top): makerSig, C1, C2, rawCheckpoint. The script enforces:
//  1. the input being spent is output 1 of rawCheckpoint (HASH256(raw) == spent txid, vout 1),
//     and rawCheckpoint's output 0 is OP_RETURN <C1> (T1: sold units -> CP:1, change -> maker);
//  2. this tx's output 0 is OP_RETURN <C2> (T2: sold units -> witness vout 2);
//  3. the maker signed SHA256(C1 || C2 || P) (BIP340, checked with OP_CHECKSIGFROMSTACK), P = the
//     36-byte outpoint the checkpoint spends (= the offer VTXO): C1/C2 can not be pinned in the leaf
//     itself, because T1 spends the allocation sealed to this VTXO's outpoint, which only exists
//     after the VTXO (whose address commits to this script) is funded. P binds the signature to this
//     VTXO, so it can not be replayed on another VTXO at the same address;
//  4. output 1 pays at least price sat to makerSpk (a segwit v1 scriptPubKey).
func Covenant(makerXOnly []byte, makerSpk []byte, price int64) ([]byte, error) {
	opret := []byte{txscript.OP_RETURN, txscript.OP_DATA_32}
	return txscript.NewScriptBuilder().
		// P = the checkpoint's input 0 outpoint (bytes 5..41 of the raw tx), kept under raw
		AddOp(arkade.OP_DUP).AddInt64(CheckpointPrevoutOffset).AddInt64(36).AddOp(arkade.OP_SUBSTR).AddOp(arkade.OP_SWAP).
		// 1. checkpoint binding and T1
		AddOp(arkade.OP_DUP).AddOp(arkade.OP_HASH256).
		AddOp(arkade.OP_PUSHCURRENTINPUTINDEX).AddOp(arkade.OP_INSPECTINPUTOUTPOINT).
		AddInt64(1).AddOp(arkade.OP_EQUALVERIFY).
		AddOp(arkade.OP_EQUALVERIFY).
		AddInt64(CheckpointSpkOffset).AddInt64(34).AddOp(arkade.OP_SUBSTR).
		AddData(opret).AddInt64(4).AddOp(arkade.OP_PICK).AddOp(arkade.OP_CAT).
		AddOp(arkade.OP_EQUALVERIFY).
		// 2. T2 at output 0 of this tx
		AddInt64(0).AddOp(arkade.OP_INSPECTOUTPUTSCRIPTPUBKEY).
		AddInt64(-1).AddOp(arkade.OP_EQUALVERIFY).
		AddData(opret).AddInt64(3).AddOp(arkade.OP_PICK).AddOp(arkade.OP_CAT).AddOp(arkade.OP_SHA256).
		AddOp(arkade.OP_EQUALVERIFY).
		// 3. maker's signature over SHA256(C1 || C2 || P)
		AddOp(arkade.OP_TOALTSTACK).AddOp(arkade.OP_CAT).AddOp(arkade.OP_FROMALTSTACK).AddOp(arkade.OP_CAT).AddOp(arkade.OP_SHA256).
		AddData(makerXOnly).AddOp(arkade.OP_CHECKSIGFROMSTACK).AddOp(arkade.OP_VERIFY).
		// 4. the price
		AddInt64(1).AddOp(arkade.OP_INSPECTOUTPUTVALUE).AddInt64(price).
		AddOp(arkade.OP_GREATERTHANOREQUAL).AddOp(arkade.OP_VERIFY).
		AddInt64(1).AddOp(arkade.OP_INSPECTOUTPUTSCRIPTPUBKEY).
		AddInt64(1).AddOp(arkade.OP_EQUALVERIFY).
		AddData(makerSpk[2:]).AddOp(arkade.OP_EQUAL).
		Script()
}
