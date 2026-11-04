// Probe: how stock arkd (pkg/ark-lib, v0.9.15) treats an RGB tapret commitment
// on the outputs it validates or builds. Tapret construction follows
// rgb-consensus 0.11.1-rc.11 src/dbc/tapret (tapscript.rs, xonlypk.rs):
// leaf = 29*OP_RESERVED || OP_RETURN || PUSH33 || mpc(32) || nonce(1),
// new root = TapBranch(leaf, original_root), output key = taptweak(internal, root).
package tapretprobe

import (
	"bytes"
	"encoding/hex"
	"testing"

	arklib "github.com/arkade-os/arkd/pkg/ark-lib"
	"github.com/arkade-os/arkd/pkg/ark-lib/offchain"
	"github.com/arkade-os/arkd/pkg/ark-lib/script"
	"github.com/arkade-os/arkd/pkg/ark-lib/tree"
	"github.com/btcsuite/btcd/btcec/v2"
	"github.com/btcsuite/btcd/btcec/v2/schnorr"
	"github.com/btcsuite/btcd/btcutil"
	"github.com/btcsuite/btcd/chaincfg"
	"github.com/btcsuite/btcd/chaincfg/chainhash"
	"github.com/btcsuite/btcd/txscript"
	"github.com/btcsuite/btcd/wire"
	"github.com/btcsuite/btcwallet/waddrmgr"
)

func tapretLeaf(mpc [32]byte, nonce byte) []byte {
	s := bytes.Repeat([]byte{txscript.OP_RESERVED}, 29)
	s = append(s, txscript.OP_RETURN, txscript.OP_DATA_33)
	s = append(s, mpc[:]...)
	return append(s, nonce)
}

func key(t *testing.T, b byte) *btcec.PublicKey {
	t.Helper()
	k, _ := btcec.PrivKeyFromBytes(bytes.Repeat([]byte{b}, 32))
	return k.PubKey()
}

// tapretTweak returns (leaf script, tweaked output key, merkle root) with the
// commitment leaf as depth-1 sibling of the original root, grinding the nonce
// so that the commitment hash sorts first (rgb-consensus check_ordering).
func tapretTweak(internal *btcec.PublicKey, origRoot chainhash.Hash) ([]byte, *btcec.PublicKey, chainhash.Hash) {
	var mpc [32]byte
	copy(mpc[:], bytes.Repeat([]byte{0xab}, 32))
	for nonce := 0; nonce < 256; nonce++ {
		leaf := tapretLeaf(mpc, byte(nonce))
		lh := txscript.NewBaseTapLeaf(leaf).TapHash()
		if bytes.Compare(lh[:], origRoot[:]) > 0 {
			continue
		}
		root := txscript.NewTapBranch(txscript.NewBaseTapLeaf(leaf), hashNode(origRoot)).TapHash()
		return leaf, txscript.ComputeTaprootOutputKey(internal, root[:]), root
	}
	panic("no nonce")
}

type hashNode chainhash.Hash

func (h hashNode) TapHash() chainhash.Hash { return chainhash.Hash(h) }
func (h hashNode) Left() txscript.TapNode  { return nil }
func (h hashNode) Right() txscript.TapNode { return nil }

func defaultVtxo(t *testing.T) (*script.TapscriptsVtxoScript, []string, *btcec.PublicKey, chainhash.Hash) {
	owner, signer := key(t, 1), key(t, 2)
	vs := script.NewDefaultVtxoScript(owner, signer, arklib.RelativeLocktime{Type: arklib.LocktimeTypeSecond, Value: 512 * 2})
	enc, err := vs.Encode()
	if err != nil {
		t.Fatal(err)
	}
	k, tt, err := vs.TapTree()
	if err != nil {
		t.Fatal(err)
	}
	return vs, enc, k, tt.GetRoot()
}

// 1. A VTXO whose key carries a tapret tweak cannot be spent through arkd:
// revealing the commitment leaf fails closure decoding (OP_SUCCESS80), and
// hiding it makes the key derived from the tapscripts differ from the VTXO key.
// Mirrors SubmitOffchainTx / validateVtxoInput: ParseVtxoScript + TapTree + key compare.
func TestTapretVtxoRejectedByArkdScriptValidation(t *testing.T) {
	_, enc, plainKey, root := defaultVtxo(t)
	leaf, tweakedKey, _ := tapretTweak(script.UnspendableKey(), root)

	if _, err := script.ParseVtxoScript(append(enc, hex.EncodeToString(leaf))); err == nil {
		t.Fatal("expected ParseVtxoScript to reject the tapret leaf")
	} else {
		t.Logf("revealed tapret leaf -> %v", firstLine(err.Error()))
	}
	if _, err := script.DecodeClosure(leaf); err == nil {
		t.Fatal("expected DecodeClosure to reject OP_SUCCESS")
	}

	vs, err := script.ParseVtxoScript(enc)
	if err != nil {
		t.Fatal(err)
	}
	k, _, _ := vs.TapTree()
	if !bytes.Equal(schnorr.SerializePubKey(k), schnorr.SerializePubKey(plainKey)) {
		t.Fatal("sanity")
	}
	if bytes.Equal(schnorr.SerializePubKey(k), schnorr.SerializePubKey(tweakedKey)) {
		t.Fatal("tweaked key unexpectedly equals the key arkd derives")
	}
	t.Logf("hidden tapret leaf -> arkd derives %x, VTXO key is %x: mismatch",
		schnorr.SerializePubKey(k), schnorr.SerializePubKey(tweakedKey))
}

// 2. Stock arkd accepts a tapret-tweaked P2TR as an ark tx *output*: it only
// checks amount bounds and that the script is P2TR (validateOffchainTxOutputs).
func TestTapretOutputIsPlainP2TR(t *testing.T) {
	_, _, _, root := defaultVtxo(t)
	_, tweakedKey, _ := tapretTweak(script.UnspendableKey(), root)
	pk, _ := script.P2TRScript(tweakedKey)
	if txscript.GetScriptClass(pk) != txscript.WitnessV1TaprootTy {
		t.Fatal("not P2TR")
	}
}

// 3. Checkpoint: arkd rebuilds the checkpoint from (VTXO, checkpoint tapscript)
// and compares txids; a tapret tweak on vout 0 changes the txid.
func TestTapretCheckpointBreaksRebuild(t *testing.T) {
	vs, enc, plainKey, _ := defaultVtxo(t)
	signer := key(t, 2)
	unroll := &script.CSVMultisigClosure{
		MultisigClosure: script.MultisigClosure{PubKeys: []*btcec.PublicKey{signer}},
		Locktime:        arklib.RelativeLocktime{Type: arklib.LocktimeTypeSecond, Value: 512},
	}
	unrollScript, _ := unroll.Script()
	_, tt, _ := vs.TapTree()
	collab, _ := vs.Closures[1].Script()
	proof, err := tt.GetTaprootMerkleProof(txscript.NewBaseTapLeaf(collab).TapHash())
	if err != nil {
		t.Fatal(err)
	}
	cb, _ := txscript.ParseControlBlock(proof.ControlBlock)
	vtxoPk, _ := script.P2TRScript(plainKey)
	in := offchain.VtxoInput{
		Outpoint:           &wire.OutPoint{Index: 0},
		Amount:             10_000,
		Tapscript:          &waddrmgr.Tapscript{ControlBlock: cb, RevealedScript: proof.Script},
		RevealedTapscripts: enc,
	}
	_, cps, err := offchain.BuildTxs([]offchain.VtxoInput{in}, []*wire.TxOut{{Value: 10_000, PkScript: vtxoPk}}, unrollScript)
	if err != nil {
		t.Fatal(err)
	}
	cp := cps[0].UnsignedTx
	if !txscript.IsPayToTaproot(cp.TxOut[0].PkScript) {
		t.Fatal("checkpoint vout0 should be P2TR")
	}
	cpScript := script.TapscriptsVtxoScript{Closures: []script.Closure{unroll, vs.Closures[1]}}
	_, cpTree, _ := cpScript.TapTree()
	_, tweakedKey, _ := tapretTweak(script.UnspendableKey(), cpTree.GetRoot())
	mod := cp.Copy()
	mod.TxOut[0].PkScript, _ = script.P2TRScript(tweakedKey)
	if mod.TxHash() == cp.TxHash() {
		t.Fatal("expected txid change")
	}
	t.Logf("checkpoint: arkd rebuild %s vs tapret %s -> CHECKPOINT_MISMATCH", cp.TxHash(), mod.TxHash())
}

// 4. Forfeit: arkd's forfeit output is P2WPKH (txBuilder.getForfeitScript) and
// the only other output is P2A, so the forfeit tx has no taproot output to
// host a tapret commitment (rgb-consensus: NoDbcOutput / NoTaprootOutput).
func TestForfeitHasNoTaprootOutput(t *testing.T) {
	op := key(t, 3)
	addr, _ := btcutil.NewAddressWitnessPubKeyHash(btcutil.Hash160(op.SerializeCompressed()), &chaincfg.RegressionNetParams)
	fs, _ := txscript.PayToAddrScript(addr)
	_, _, plainKey, _ := defaultVtxo(t)
	vtxoPk, _ := script.P2TRScript(plainKey)
	connPk, _ := script.P2TRScript(key(t, 4))
	ptx, err := tree.BuildForfeitTx(
		[]*wire.OutPoint{{Index: 0}, {Index: 1}},
		[]uint32{wire.MaxTxInSequenceNum, wire.MaxTxInSequenceNum},
		[]*wire.TxOut{{Value: 10_000, PkScript: vtxoPk}, {Value: 330, PkScript: connPk}},
		fs, 0,
	)
	if err != nil {
		t.Fatal(err)
	}
	for i, o := range ptx.UnsignedTx.TxOut {
		if txscript.IsPayToTaproot(o.PkScript) {
			t.Fatalf("unexpected P2TR at vout %d", i)
		}
		t.Logf("forfeit vout %d: %x", i, o.PkScript)
	}
}

// 5. The tapret leaf is an OP_SUCCESS80 tapscript: whoever knows the tapret
// proof (internal key + sibling) can spend the output by script path. Valid
// under consensus flags, rejected only by relay policy (DiscourageOpSuccess).
func TestTapretLeafSpendableWithoutSignature(t *testing.T) {
	_, _, _, root := defaultVtxo(t)
	internal := script.UnspendableKey()
	leaf, tweakedKey, _ := tapretTweak(internal, root)
	pk, _ := script.P2TRScript(tweakedKey)
	prevOut := &wire.TxOut{Value: 10_000, PkScript: pk}

	tx := wire.NewMsgTx(3)
	tx.AddTxIn(&wire.TxIn{PreviousOutPoint: wire.OutPoint{Index: 0}, Sequence: wire.MaxTxInSequenceNum})
	thief, _ := script.P2TRScript(key(t, 9))
	tx.AddTxOut(&wire.TxOut{Value: 9_000, PkScript: thief})

	// control block: internal key, parity of output key, sibling = original root
	tweakedFull := txscript.ComputeTaprootOutputKey(internal, mustRoot(leaf, root))
	cb := txscript.ControlBlock{
		InternalKey:     internal,
		OutputKeyYIsOdd: tweakedFull.SerializeCompressed()[0] == 0x03,
		LeafVersion:     txscript.BaseLeafVersion,
		InclusionProof:  root[:],
	}
	cbBytes, _ := cb.ToBytes()
	tx.TxIn[0].Witness = wire.TxWitness{leaf, cbBytes}

	fetcher := txscript.NewCannedPrevOutputFetcher(prevOut.PkScript, prevOut.Value)
	hashes := txscript.NewTxSigHashes(tx, fetcher)
	consensus := txscript.ScriptBip16 | txscript.ScriptVerifyWitness | txscript.ScriptVerifyTaproot |
		txscript.ScriptVerifyCheckLockTimeVerify | txscript.ScriptVerifyCheckSequenceVerify
	run := func(flags txscript.ScriptFlags) error {
		vm, err := txscript.NewEngine(prevOut.PkScript, tx, 0, flags, nil, hashes, prevOut.Value, fetcher)
		if err != nil {
			return err
		}
		return vm.Execute()
	}
	if err := run(consensus); err != nil {
		t.Fatalf("consensus flags: expected success, got %v", err)
	}
	t.Log("consensus flags: tapret leaf spend with no signature -> VALID")
	if err := run(txscript.StandardVerifyFlags); err == nil {
		t.Fatal("standard flags: expected rejection")
	} else {
		t.Logf("standard (relay) flags: %v", err)
	}
}

func mustRoot(leaf []byte, orig chainhash.Hash) []byte {
	r := txscript.NewTapBranch(txscript.NewBaseTapLeaf(leaf), hashNode(orig)).TapHash()
	return r[:]
}

// 6. Tree shape: arkd's TapTree() uses AssembleTaprootScriptTree over the
// closure list. For the 2-leaf default VTXO a third leaf lands at depth 1
// (the tapret shape); for 3+ closures it does not, so a patched arkd would
// need an explicit "extra sibling" rather than an extra closure.
func TestAssembleShape(t *testing.T) {
	_, enc, _, root := defaultVtxo(t)
	leaf, _, want := tapretTweak(script.UnspendableKey(), root)
	leaves := []txscript.TapLeaf{}
	for _, e := range enc {
		b, _ := hex.DecodeString(e)
		leaves = append(leaves, txscript.NewBaseTapLeaf(b))
	}
	got := txscript.AssembleTaprootScriptTree(append(leaves, txscript.NewBaseTapLeaf(leaf))...).RootNode.TapHash()
	t.Logf("2 closures + tapret via AssembleTaprootScriptTree matches tapret root: %v", got == want)
	extra, _ := (&script.MultisigClosure{PubKeys: []*btcec.PublicKey{key(t, 5), key(t, 2)}}).Script()
	l3 := append(append([]txscript.TapLeaf{}, leaves...), txscript.NewBaseTapLeaf(extra))
	r3 := txscript.AssembleTaprootScriptTree(l3...).RootNode.TapHash()
	_, _, want3 := tapretTweak(script.UnspendableKey(), r3)
	got3 := txscript.AssembleTaprootScriptTree(append(l3, txscript.NewBaseTapLeaf(leaf))...).RootNode.TapHash()
	t.Logf("3 closures + tapret matches tapret root: %v", got3 == want3)
}

func firstLine(s string) string {
	if i := bytes.IndexByte([]byte(s), '\n'); i > 0 {
		return s[:i]
	}
	return s
}

// 7. Vector for the cross-check against rgb-consensus (Rust): leaf hashes of
// the default VTXO closures, nonce, and the output key computed above.
func TestVectorForRust(t *testing.T) {
	_, enc, _, root := defaultVtxo(t)
	leaf, tweakedKey, _ := tapretTweak(script.UnspendableKey(), root)
	for _, e := range enc {
		b, _ := hex.DecodeString(e)
		h := txscript.NewBaseTapLeaf(b).TapHash()
		t.Logf("LEAF %x", h[:])
	}
	t.Logf("NONCE %d", leaf[63])
	t.Logf("INTERNAL %x", schnorr.SerializePubKey(script.UnspendableKey()))
	t.Logf("OUTKEY %x", schnorr.SerializePubKey(tweakedKey))
}
