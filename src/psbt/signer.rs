// LNP/BP Core Library implementing LNPBP specifications & standards
// Written in 2019 by
//     Dr. Maxim Orlovsky <orlovsky@pandoracore.com>
//
// To the extent possible under law, the author(s) have dedicated all
// copyright and related and neighboring rights to this software to
// the public domain worldwide. This software is distributed without
// any warranty.
//
// You should have received a copy of the MIT License
// along with this software.
// If not, see <https://opensource.org/licenses/MIT>.

use amplify::Wrapper;
use bitcoin::secp256k1::constants::SECRET_KEY_SIZE;
use bitcoin::util::bip32::ExtendedPrivKey;
use bitcoin::{PublicKey, SigHashType, Txid};

use crate::psbt::raw::ProprietaryKey;
use crate::script::WitnessScript;
use crate::Psbt;
use crate::{PubkeyScript, RedeemScript, ToP2pkh};

// TODO: Derive `Ord`, `Hash` once `SigHashType` will support it
#[derive(Copy, Clone, Eq, PartialEq, Debug, Display, Error)]
#[display(doc_comments)]
pub enum SigningError {
    /// Provided `non_witness_utxo` TXID {1} does not match `prev_out` from the
    /// transaction input #{0}
    WrongInputTxid(usize, Txid),

    /// Input #{0} requires custom sighash type `{1}`, while only `SIGHASH_ALL`
    /// is allowed
    SigHashType(usize, SigHashType),

    /// No redeem or witness script specified for input #{0}
    NoPrevoutScript(usize),

    /// Input #{0} spending witness output does not contain witness script
    /// source
    NoWitnessScript(usize),

    /// Input #{0} must be a witness input since it is supplied with
    /// `witness_utxo` data and does not have `non_witness_utxo`
    NonWitnessInput(usize),

    /// Unable to derive private key with a given derivation path: elliptic
    /// curve prime field order (`p`) overflow or derivation resulting at the
    /// point-at-infinity.
    SecpPrivkeyDerivation(usize),

    /// `scriptPubkey` from previous output does not match witness or redeem
    /// script from the same input #{0} supplied in PSBT
    ScriptPubkeyMismatch(usize),

    /// Wrong pay-to-contract public key tweak data length in input #{input}:
    /// {len} bytes instead of 32
    WrongTweakLength { input: usize, len: usize },

    /// Error applying tweak matching public key {1} from input #{0}: the tweak
    /// value is either a modulo-negation of the original private key, or
    /// it leads to elliptic curve prime field order (`p`) overflow
    TweakFailure(usize, PublicKey),
}

pub trait Signer {
    fn sign(
        &mut self,
        master_xpriv: ExtendedPrivKey,
        wipe: bool,
    ) -> Result<usize, SigningError>;
}

impl Signer for Psbt {
    fn sign(
        &mut self,
        mut master_xpriv: ExtendedPrivKey,
        wipe: bool,
    ) -> Result<usize, SigningError> {
        let master_fingerprint = master_xpriv.fingerprint(&crate::SECP256K1);
        let mut signature_count = 0usize;
        let tx = &self.global.unsigned_tx;
        for (index, inp) in self.inputs.iter_mut().enumerate() {
            let txin = tx.input[index].clone();
            for (pubkey, (fingerprint, derivation)) in &inp.bip32_derivation {
                if *fingerprint != master_fingerprint {
                    continue;
                }

                let xpriv = master_xpriv
                    .derive_priv(&crate::SECP256K1, &derivation)
                    .map_err(|_| SigningError::SecpPrivkeyDerivation(index))?;
                if *pubkey != xpriv.private_key.public_key(&crate::SECP256K1) {
                    continue;
                }

                // Extract & check previous output information
                let (script_pubkey, require_witness) =
                    match (&inp.non_witness_utxo, &inp.witness_utxo) {
                        (Some(prev_tx), _) => {
                            if tx.txid() != txin.previous_output.txid {
                                Err(SigningError::WrongInputTxid(
                                    index,
                                    txin.previous_output.txid,
                                ))?
                            }
                            let prevout = prev_tx.output
                                [txin.previous_output.vout as usize]
                                .clone();
                            (prevout.script_pubkey, false)
                        }
                        (None, Some(txout)) => {
                            (txout.script_pubkey.clone(), true)
                        }
                        _ => continue,
                    };
                let script_pubkey = PubkeyScript::from_inner(script_pubkey);

                if let Some(sighash_type) = inp.sighash_type {
                    if sighash_type != SigHashType::All {
                        Err(SigningError::SigHashType(index, sighash_type))?
                    }
                }

                // Check script_pubkey match
                if let Some(ref witness_script) = inp.witness_script {
                    let witness_script: WitnessScript =
                        WitnessScript::from_inner(witness_script.clone());
                    if script_pubkey != witness_script.to_p2wsh()
                        && script_pubkey != witness_script.to_p2sh_wsh()
                    {
                        Err(SigningError::ScriptPubkeyMismatch(index))?;
                    }
                } else if let Some(ref redeem_script) = inp.redeem_script {
                    if require_witness {
                        Err(SigningError::NoWitnessScript(index))?
                    }
                    let redeem_script: RedeemScript =
                        RedeemScript::from_inner(redeem_script.clone());
                    if script_pubkey != redeem_script.to_p2sh() {
                        Err(SigningError::ScriptPubkeyMismatch(index))?;
                    }
                } else {
                    if script_pubkey != pubkey.to_p2pkh() {
                        if require_witness {
                            Err(SigningError::NonWitnessInput(index))?
                        }
                    } else if script_pubkey != pubkey.to_p2wpkh()
                        && script_pubkey != pubkey.to_p2sh_wpkh()
                    {
                        Err(SigningError::NoPrevoutScript(index))?;
                    }
                }

                let mut priv_key = xpriv.private_key.key;

                // Apply tweak, if any
                if let Some(tweak) = inp.proprietary.get(&ProprietaryKey {
                    prefix: b"P2C".to_vec(),
                    subtype: 0,
                    key: pubkey.to_bytes(),
                }) {
                    if tweak.len() != SECRET_KEY_SIZE {
                        Err(SigningError::WrongTweakLength {
                            input: index,
                            len: tweak.len(),
                        })?
                    }
                    priv_key.add_assign(&tweak).map_err(|_| {
                        SigningError::TweakFailure(index, *pubkey)
                    })?;
                }

                let sig_hash = tx.signature_hash(
                    index,
                    &script_pubkey,
                    SigHashType::All.as_u32(),
                );
                let signature = crate::SECP256K1.sign(
                    &bitcoin::secp256k1::Message::from_slice(&sig_hash[..])
                        .expect("SigHash generation is broken"),
                    &priv_key,
                );
                unsafe {
                    priv_key.as_mut_ptr().copy_from(
                        [0u8; SECRET_KEY_SIZE].as_ptr(),
                        SECRET_KEY_SIZE,
                    )
                };

                let mut partial_sig = signature.serialize_der().to_vec();
                partial_sig.push(SigHashType::All.as_u32() as u8);
                inp.sighash_type = Some(SigHashType::All);
                inp.partial_sigs.insert(*pubkey, partial_sig);
                signature_count += 1;
            }
        }

        if wipe {
            unsafe {
                master_xpriv
                    .private_key
                    .key
                    .as_mut_ptr()
                    .copy_from([0u8; SECRET_KEY_SIZE].as_ptr(), SECRET_KEY_SIZE)
            };
        }

        Ok(signature_count)
    }
}