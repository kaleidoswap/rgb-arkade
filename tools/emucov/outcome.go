package main

import (
	"bytes"
	"encoding/binary"

	"github.com/arkade-os/emulator/pkg/arkade"
	"github.com/btcsuite/btcd/chainhash/v2"
	"github.com/btcsuite/btcd/txscript/v2"
)

// Outcome leaf layout (fixed 232 bytes). A zero key / zero number disables that check.
const (
	OffC1         = 0   // 32: T1 commitment, the checkpoint's OP_RETURN (spends the covenant VTXO)
	OffC2         = 32  // 32: T2 commitment, ark tx output 0 OP_RETURN
	OffPayKey     = 64  // 32: output 1 must be P2TR(payKey) ...
	OffPayMin     = 96  // 8 LE: ... with value >= payMin
	OffNextKey    = 104 // 32: output 3 must be P2TR(nextKey) (the next covenant VTXO) ...
	OffNextMin    = 136 // 8 LE: ... with value >= nextMin
	OffRgbKey     = 144 // 32: output 2 (the RGB witness-vout seal) must be P2TR(rgbKey)
	OffAuthKey    = 176 // 32: BIP340 signature by authKey (CHECKSIGFROMSTACK) required, see below
	OffNotBefore  = 208 // 8 LE: unix time, emulator clock must be >= notBefore
	OffNotAfter   = 216 // 8 LE: unix time, emulator clock must be < notAfter
	OffPriceBelow = 224 // 8 LE: oracle mode: authKey signed stmt = price(8 LE)||ts(8 LE), price < priceBelow
	LeafLen       = 232

	// OracleMaxAge: an oracle statement is usable for this many seconds after its timestamp.
	OracleMaxAge = 3600
)

var (
	LeafTag   = []byte("ArkRGB/outcome")
	BranchTag = []byte("ArkRGB/branch")
	zero32    = make([]byte, 32)
)

// Outcome is one precommitted way of spending the covenant VTXO.
type Outcome struct {
	C1, C2                  [32]byte
	PayKey, NextKey, RgbKey [32]byte
	AuthKey                 [32]byte
	PayMin, NextMin         uint64
	NotBefore, NotAfter     uint64
	PriceBelow              uint64
}

func (o Outcome) Bytes() []byte {
	b := make([]byte, LeafLen)
	copy(b[OffC1:], o.C1[:])
	copy(b[OffC2:], o.C2[:])
	copy(b[OffPayKey:], o.PayKey[:])
	binary.LittleEndian.PutUint64(b[OffPayMin:], o.PayMin)
	copy(b[OffNextKey:], o.NextKey[:])
	binary.LittleEndian.PutUint64(b[OffNextMin:], o.NextMin)
	copy(b[OffRgbKey:], o.RgbKey[:])
	copy(b[OffAuthKey:], o.AuthKey[:])
	binary.LittleEndian.PutUint64(b[OffNotBefore:], o.NotBefore)
	binary.LittleEndian.PutUint64(b[OffNotAfter:], o.NotAfter)
	binary.LittleEndian.PutUint64(b[OffPriceBelow:], o.PriceBelow)
	return b
}

// LeafHash = TaggedHash(LeafTag, leaf), as OP_MERKLEBRANCHROOT computes it.
func LeafHash(leaf []byte) []byte { h := chainhash.TaggedHash(LeafTag, leaf); return h[:] }

func branch(a, b []byte) []byte {
	if bytes.Compare(a, b) > 0 {
		a, b = b, a
	}
	h := chainhash.TaggedHash(BranchTag, append(append([]byte{}, a...), b...))
	return h[:]
}

// MerkleTree returns the root and, per leaf, its proof (concatenated siblings, leaf to root).
// An unpaired node is promoted unchanged (no sibling for that level).
func MerkleTree(leaves [][]byte) ([]byte, [][]byte) {
	level := make([][]byte, len(leaves))
	pos := make([]int, len(leaves))
	proofs := make([][]byte, len(leaves))
	for i, l := range leaves {
		level[i] = LeafHash(l)
		pos[i] = i
	}
	for len(level) > 1 {
		var next [][]byte
		for i := 0; i < len(level); i += 2 {
			if i+1 < len(level) {
				next = append(next, branch(level[i], level[i+1]))
			} else {
				next = append(next, level[i])
			}
		}
		for j := range leaves {
			p := pos[j]
			if s := p ^ 1; s < len(level) {
				proofs[j] = append(proofs[j], level[s]...)
			}
			pos[j] = p / 2
		}
		level = next
	}
	return level[0], proofs
}

// OutcomeCovenant: the emulator-run fill-leaf script choosing one of the precommitted outcomes
// whose Merkle root is pinned in the script.
//
// Witness (bottom -> top): stmt, authSig, proof, leaf, rawCheckpoint.
//   - the input being spent is output 1 of rawCheckpoint, whose output 0 is OP_RETURN <leaf.C1>;
//   - output 0 is OP_RETURN <leaf.C2>;
//   - payKey/payMin on output 1, rgbKey on output 2, nextKey/nextMin on output 3 (if set);
//   - notBefore <= emulator time < notAfter (if set; OP_CHECKTIME = emulator wall clock);
//   - MERKLEBRANCHROOT(leaf, proof) == root;
//   - authKey set: CHECKSIGFROMSTACK(authSig, SHA256(leaf || P), authKey), P = the 36-byte outpoint
//     of the covenant VTXO (from the checkpoint's input); oracle mode (priceBelow > 0) instead
//     verifies authSig over SHA256(stmt), stmt = price(8 LE) || ts(8 LE), price < priceBelow and
//     emulator time < ts + OracleMaxAge.
func OutcomeCovenant(root []byte) ([]byte, error) {
	return OutcomeCovenantAsset(root, nil, 0, 0)
}

// OutcomeCovenantAsset is OutcomeCovenant plus, when assetTxid is set, a check that output 1
// carries at least assetMin units of the Arkade asset (assetTxid, assetGidx) according to the
// tx's asset packet (OP_INSPECTOUTASSETLOOKUP; arkd validates the packet against the inputs).
// assetTxid is in internal byte order.
func OutcomeCovenantAsset(root, assetTxid []byte, assetGidx uint16, assetMin uint64) ([]byte, error) {
	opret := []byte{txscript.OP_RETURN, txscript.OP_DATA_32}
	b := txscript.NewScriptBuilder()
	if assetTxid != nil {
		b.AddInt64(1).AddData(assetTxid).AddInt64(int64(assetGidx)).AddOp(arkade.OP_INSPECTOUTASSETLOOKUP).
			AddOp(arkade.OP_VERIFY).AddInt64(int64(assetMin)).AddOp(arkade.OP_GREATERTHANOREQUAL).AddOp(arkade.OP_VERIFY)
	}
	// A. checkpoint binding + C1; P to the alt stack
	b.AddOp(arkade.OP_DUP).AddInt64(CheckpointPrevoutOffset).AddInt64(36).AddOp(arkade.OP_SUBSTR).AddOp(arkade.OP_SWAP).
		AddOp(arkade.OP_DUP).AddOp(arkade.OP_HASH256).
		AddOp(arkade.OP_PUSHCURRENTINPUTINDEX).AddOp(arkade.OP_INSPECTINPUTOUTPOINT).
		AddInt64(1).AddOp(arkade.OP_EQUALVERIFY).AddOp(arkade.OP_EQUALVERIFY).
		AddInt64(CheckpointSpkOffset).AddInt64(34).AddOp(arkade.OP_SUBSTR).
		AddData(opret).AddInt64(3).AddOp(arkade.OP_PICK).AddInt64(OffC1).AddInt64(32).AddOp(arkade.OP_SUBSTR).AddOp(arkade.OP_CAT).
		AddOp(arkade.OP_EQUALVERIFY).
		AddOp(arkade.OP_TOALTSTACK)
	// B. C2 at output 0
	b.AddInt64(0).AddOp(arkade.OP_INSPECTOUTPUTSCRIPTPUBKEY).AddInt64(-1).AddOp(arkade.OP_EQUALVERIFY).
		AddData(opret).AddInt64(2).AddOp(arkade.OP_PICK).AddInt64(OffC2).AddInt64(32).AddOp(arkade.OP_SUBSTR).AddOp(arkade.OP_CAT).
		AddOp(arkade.OP_SHA256).AddOp(arkade.OP_EQUALVERIFY)
	// C. pinned outputs
	key := func(off, out, minOff int) {
		b.AddOp(arkade.OP_DUP).AddInt64(int64(off)).AddInt64(32).AddOp(arkade.OP_SUBSTR).
			AddOp(arkade.OP_DUP).AddData(zero32).AddOp(arkade.OP_EQUAL).
			AddOp(arkade.OP_IF).AddOp(arkade.OP_DROP).
			AddOp(arkade.OP_ELSE).
			AddInt64(int64(out)).AddOp(arkade.OP_INSPECTOUTPUTSCRIPTPUBKEY).AddInt64(1).AddOp(arkade.OP_EQUALVERIFY).AddOp(arkade.OP_EQUALVERIFY)
		if minOff >= 0 {
			b.AddInt64(int64(out)).AddOp(arkade.OP_INSPECTOUTPUTVALUE).
				AddOp(arkade.OP_OVER).AddInt64(int64(minOff)).AddInt64(8).AddOp(arkade.OP_SUBSTR).AddOp(arkade.OP_BIN2NUM).
				AddOp(arkade.OP_GREATERTHANOREQUAL).AddOp(arkade.OP_VERIFY)
		}
		b.AddOp(arkade.OP_ENDIF)
	}
	key(OffPayKey, 1, OffPayMin)
	key(OffRgbKey, 2, -1)
	key(OffNextKey, 3, OffNextMin)
	// E. emulator clock window
	b.AddOp(arkade.OP_DUP).AddInt64(OffNotBefore).AddInt64(8).AddOp(arkade.OP_SUBSTR).AddOp(arkade.OP_BIN2NUM).
		AddOp(arkade.OP_DUP).AddOp(arkade.OP_0NOTEQUAL).
		AddOp(arkade.OP_IF).AddOp(arkade.OP_CHECKTIME).AddOp(arkade.OP_VERIFY).
		AddOp(arkade.OP_ELSE).AddOp(arkade.OP_DROP).AddOp(arkade.OP_ENDIF)
	b.AddOp(arkade.OP_DUP).AddInt64(OffNotAfter).AddInt64(8).AddOp(arkade.OP_SUBSTR).AddOp(arkade.OP_BIN2NUM).
		AddOp(arkade.OP_DUP).AddOp(arkade.OP_0NOTEQUAL).
		AddOp(arkade.OP_IF).AddOp(arkade.OP_CHECKTIME).AddOp(arkade.OP_NOT).AddOp(arkade.OP_VERIFY).
		AddOp(arkade.OP_ELSE).AddOp(arkade.OP_DROP).AddOp(arkade.OP_ENDIF)
	// F. leaf in the tree: [stmt authSig proof L] -> [stmt authSig]
	b.AddOp(arkade.OP_DUP).AddOp(arkade.OP_TOALTSTACK).
		AddData(LeafTag).AddData(BranchTag).AddOp(arkade.OP_2SWAP).
		AddOp(arkade.OP_MERKLEBRANCHROOT).AddData(root).AddOp(arkade.OP_EQUALVERIFY)
	// G. authorization: [stmt authSig L P]
	b.AddOp(arkade.OP_FROMALTSTACK).AddOp(arkade.OP_FROMALTSTACK).
		AddOp(arkade.OP_OVER).AddInt64(OffAuthKey).AddInt64(32).AddOp(arkade.OP_SUBSTR).
		AddOp(arkade.OP_DUP).AddData(zero32).AddOp(arkade.OP_EQUAL).
		AddOp(arkade.OP_IF).AddOp(arkade.OP_2DROP).AddOp(arkade.OP_NIP).
		AddOp(arkade.OP_ELSE).AddOp(arkade.OP_TOALTSTACK).
		AddOp(arkade.OP_OVER).AddInt64(OffPriceBelow).AddInt64(8).AddOp(arkade.OP_SUBSTR).AddOp(arkade.OP_BIN2NUM).AddOp(arkade.OP_0NOTEQUAL).
		AddOp(arkade.OP_IF).AddOp(arkade.OP_DROP).AddInt64(2).AddOp(arkade.OP_PICK).AddOp(arkade.OP_SHA256).
		AddOp(arkade.OP_ELSE).AddOp(arkade.OP_OVER).AddOp(arkade.OP_SWAP).AddOp(arkade.OP_CAT).AddOp(arkade.OP_SHA256).
		AddOp(arkade.OP_ENDIF).
		AddOp(arkade.OP_ROT).AddOp(arkade.OP_SWAP).AddOp(arkade.OP_FROMALTSTACK).
		AddOp(arkade.OP_CHECKSIGFROMSTACK).AddOp(arkade.OP_VERIFY).
		AddOp(arkade.OP_ENDIF)
	// H. oracle threshold + freshness: [stmt L]
	b.AddOp(arkade.OP_DUP).AddInt64(OffPriceBelow).AddInt64(8).AddOp(arkade.OP_SUBSTR).AddOp(arkade.OP_BIN2NUM).
		AddOp(arkade.OP_DUP).AddOp(arkade.OP_0NOTEQUAL).
		AddOp(arkade.OP_IF).
		AddInt64(2).AddOp(arkade.OP_PICK).AddInt64(0).AddInt64(8).AddOp(arkade.OP_SUBSTR).AddOp(arkade.OP_BIN2NUM).
		AddOp(arkade.OP_GREATERTHAN).AddOp(arkade.OP_VERIFY).
		AddOp(arkade.OP_OVER).AddInt64(8).AddInt64(8).AddOp(arkade.OP_SUBSTR).AddOp(arkade.OP_BIN2NUM).
		AddInt64(OracleMaxAge).AddOp(arkade.OP_ADD).AddOp(arkade.OP_CHECKTIME).AddOp(arkade.OP_NOT).AddOp(arkade.OP_VERIFY).
		AddOp(arkade.OP_ELSE).AddOp(arkade.OP_DROP).AddOp(arkade.OP_ENDIF).
		AddOp(arkade.OP_2DROP).AddOp(arkade.OP_TRUE)
	return b.Script()
}
