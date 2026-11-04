package emulator

// [rgb-arkade probe] Does the emulator accept the checkpoint-opret / forfeit-opret layouts?

import (
	"bytes"
	"testing"

	arkscript "github.com/arkade-os/arkd/pkg/ark-lib/script"
	"github.com/arkade-os/arkd/pkg/ark-lib/txutils"
	"github.com/btcsuite/btcd/btcec/v2"
	"github.com/btcsuite/btcd/chainhash/v2"
	"github.com/btcsuite/btcd/psbt/v2"
	"github.com/btcsuite/btcd/txscript/v2"
	"github.com/btcsuite/btcd/wire/v2"
	"github.com/stretchr/testify/require"
)

func rgbOpret() *wire.TxOut {
	pk, _ := txscript.NullDataScript(bytes.Repeat([]byte{0xAB}, 32))
	return &wire.TxOut{Value: 0, PkScript: pk}
}

// layout: "stock" = [cp, anchor]; "opret0" = [opret, cp, anchor] (our checkpoint-opret patch);
// "opretLast" = [cp, anchor, opret]
func runCheckpointLayout(t *testing.T, layout string) error {
	k1, _ := btcec.NewPrivateKey()
	arkdKey, _ := btcec.NewPrivateKey()
	unroll := testArkdCheckpointTapscript(t, arkdKey.PubKey())
	closure := &arkscript.MultisigClosure{PubKeys: []*btcec.PublicKey{k1.PubKey()}}
	vs := arkscript.TapscriptsVtxoScript{Closures: []arkscript.Closure{closure}}
	tapKey, tapTree, err := vs.TapTree()
	require.NoError(t, err)
	vtxoPk, _ := arkscript.P2TRScript(tapKey)
	leafScript, _ := closure.Script()
	leaf := txscript.NewBaseTapLeaf(leafScript)
	proof, _ := tapTree.GetTaprootMerkleProof(leaf.TapHash())
	field := &psbt.TaprootTapLeafScript{ControlBlock: proof.ControlBlock, Script: proof.Script, LeafVersion: txscript.BaseLeafVersion}

	const amount = int64(100_000)
	prevOut := &wire.TxOut{Value: amount, PkScript: vtxoPk}
	prevTx := wire.NewMsgTx(3)
	prevTx.AddTxIn(&wire.TxIn{PreviousOutPoint: wire.OutPoint{Hash: chainhash.Hash{9}}})
	prevTx.AddTxOut(prevOut)

	cpPk, arkField := testCheckpointSpend(t, unroll, field.Script)
	cpOut := &wire.TxOut{Value: amount, PkScript: cpPk}
	cp := wire.NewMsgTx(3)
	cp.AddTxIn(&wire.TxIn{PreviousOutPoint: wire.OutPoint{Hash: prevTx.TxHash()}})
	var cpVout uint32
	switch layout {
	case "stock":
		cp.AddTxOut(cpOut)
		cp.AddTxOut(txutils.AnchorOutput())
	case "opret0":
		cp.AddTxOut(rgbOpret())
		cp.AddTxOut(cpOut)
		cp.AddTxOut(txutils.AnchorOutput())
		cpVout = 1
	case "opretLast":
		cp.AddTxOut(cpOut)
		cp.AddTxOut(txutils.AnchorOutput())
		cp.AddTxOut(rgbOpret())
	}
	cpPtx, _ := psbt.NewFromUnsignedTx(cp)
	cpPtx.Inputs[0].WitnessUtxo = &wire.TxOut{Value: amount, PkScript: vtxoPk}
	cpPtx.Inputs[0].TaprootLeafScript = []*psbt.TaprootTapLeafScript{field}

	ark := wire.NewMsgTx(3)
	ark.AddTxIn(&wire.TxIn{PreviousOutPoint: wire.OutPoint{Hash: cp.TxHash(), Index: cpVout}})
	arkPtx, _ := psbt.NewFromUnsignedTx(ark)
	arkPtx.Inputs[0].WitnessUtxo = &wire.TxOut{Value: amount, PkScript: cpPk}
	arkPtx.Inputs[0].TaprootLeafScript = []*psbt.TaprootTapLeafScript{arkField}

	return validateCheckpoint(arkPtx, 0, cpPtx, prevOut, leaf, unroll)
}

func TestRGBProbeCheckpointLayouts(t *testing.T) {
	for _, l := range []string{"stock", "opret0", "opretLast"} {
		err := runCheckpointLayout(t, l)
		t.Logf("checkpoint layout %-9s -> err=%v", l, err)
	}
}

func TestRGBProbeForfeitLayout(t *testing.T) {
	vtxoPk, _ := txscript.PayToTaprootScript(func() *btcec.PublicKey { k, _ := btcec.NewPrivateKey(); return k.PubKey() }())
	connPk, _ := txscript.PayToTaprootScript(func() *btcec.PublicKey { k, _ := btcec.NewPrivateKey(); return k.PubKey() }())
	forfeitPk, _ := txscript.PayToTaprootScript(func() *btcec.PublicKey { k, _ := btcec.NewPrivateKey(); return k.PubKey() }())
	vtxo := &wire.TxOut{Value: 10_000, PkScript: vtxoPk}
	conn := &wire.TxOut{Value: 330, PkScript: connPk}
	mk := func(withOpret bool) *psbt.Packet {
		tx := wire.NewMsgTx(3)
		tx.AddTxIn(&wire.TxIn{PreviousOutPoint: wire.OutPoint{Hash: chainhash.Hash{1}}})
		tx.AddTxIn(&wire.TxIn{PreviousOutPoint: wire.OutPoint{Hash: chainhash.Hash{2}}})
		if withOpret { // our forfeit-opret patch: opret at index 0
			tx.AddTxOut(rgbOpret())
		}
		tx.AddTxOut(&wire.TxOut{Value: vtxo.Value + conn.Value - txutils.ANCHOR_VALUE, PkScript: forfeitPk})
		tx.AddTxOut(txutils.AnchorOutput())
		p, _ := psbt.NewFromUnsignedTx(tx)
		p.Inputs[0].WitnessUtxo = vtxo
		p.Inputs[1].WitnessUtxo = conn
		return p
	}
	t.Logf("forfeit stock      -> err=%v", validateForfeitOutputs(mk(false), 0, vtxo, conn, forfeitPk))
	t.Logf("forfeit with opret -> err=%v", validateForfeitOutputs(mk(true), 0, vtxo, conn, forfeitPk))
}
