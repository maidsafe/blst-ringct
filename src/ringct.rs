use bls_bulletproofs::{
    blstrs::{G1Affine, G1Projective, Scalar},
    group::ff::Field,
    group::Curve,
    group::GroupEncoding,
    merlin::Transcript,
    rand::{CryptoRng, RngCore},
    BulletproofGens, PedersenGens, RangeProof,
};
use std::collections::BTreeSet;
use tiny_keccak::{Hasher, Sha3};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use crate::{Error, MlsagMaterial, MlsagSignature, Result, RevealedCommitment};
pub(crate) const RANGE_PROOF_BITS: usize = 64; // note: Range Proof max-bits is 64. allowed are: 8, 16, 32, 64 (only)
                                               //       This limits our amount field to 64 bits also.
pub(crate) const RANGE_PROOF_PARTIES: usize = 1; // The maximum number of parties that can produce an aggregated proof
pub(crate) const MERLIN_TRANSCRIPT_LABEL: &[u8] = b"BLST_RINGCT";

/// Represents a Dbc's value.
pub type Amount = u64;

#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone)]
pub struct Output {
    pub public_key: G1Affine,
    pub amount: Amount,
}

impl Output {
    pub fn new<G: Into<G1Affine>>(public_key: G, amount: Amount) -> Self {
        Self {
            public_key: public_key.into(),
            amount,
        }
    }

    pub fn public_key(&self) -> G1Affine {
        self.public_key
    }

    pub fn amount(&self) -> Amount {
        self.amount
    }

    /// Generate a commitment to the input amount
    pub fn random_commitment(&self, rng: impl RngCore) -> RevealedCommitment {
        RevealedCommitment::from_value(self.amount, rng)
    }
}

#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone)]
struct RevealedOutputCommitment {
    pub public_key: G1Affine,
    pub revealed_commitment: RevealedCommitment,
}

#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone, Default)]
pub struct RingCtMaterial {
    pub inputs: Vec<MlsagMaterial>,
    pub outputs: Vec<Output>,
}

impl RingCtMaterial {
    pub fn sign(
        &self,
        mut rng: impl RngCore + CryptoRng,
    ) -> Result<(RingCtTransaction, Vec<RevealedCommitment>)> {
        // We need to gather a bunch of things for our message to sign.
        //   All public keys in all (input) rings
        //   All key-images,
        //   All PseudoCommitments
        //   All output public keys.
        //   All output commitments
        //   All output range proofs
        //
        //   notes:
        //     1. the real pk is randomly mixed with decoys by MlsagMaterial
        //     2. output commitments, range_proofs, and public_keys are bundled
        //        together in OutputProofs
        //     3. all these must be generated in proper order. It would be nice
        //        to make RingCtMaterial deterministic by instantiating with a seed.
        let revealed_pseudo_commitments = self.revealed_pseudo_commitments(&mut rng);
        let pseudo_commitments = self.pseudo_commitments(&revealed_pseudo_commitments);
        let revealed_output_commitments =
            self.revealed_output_commitments(&revealed_pseudo_commitments, &mut rng);
        let output_proofs = self.output_range_proofs(&revealed_output_commitments, &mut rng)?;

        // Generate message to sign.
        // note: must match message generated by RingCtTransaction::verify()
        let msg = gen_message_for_signing(
            &self.public_keys(),
            &self.key_images(),
            &pseudo_commitments,
            &output_proofs,
        );

        // We create a ring signature for each input
        let mlsags: Vec<MlsagSignature> = self
            .inputs
            .iter()
            .zip(revealed_pseudo_commitments.iter())
            .map(|(m, r)| m.sign(&msg, r, &Self::pc_gens()))
            .collect();

        let revealed_output_commitments = revealed_output_commitments
            .iter()
            .map(|r| r.revealed_commitment)
            .collect::<Vec<_>>();

        Ok((
            RingCtTransaction {
                mlsags,
                outputs: output_proofs,
            },
            revealed_output_commitments,
        ))
    }

    fn bp_gens() -> BulletproofGens {
        BulletproofGens::new(RANGE_PROOF_BITS, RANGE_PROOF_PARTIES)
    }

    fn pc_gens() -> PedersenGens {
        Default::default()
    }

    pub fn public_keys(&self) -> Vec<G1Affine> {
        self.inputs.iter().flat_map(|m| m.public_keys()).collect()
    }

    pub fn key_images(&self) -> Vec<G1Affine> {
        self.inputs
            .iter()
            .map(|m| m.true_input.key_image().to_affine())
            .collect()
    }

    fn revealed_pseudo_commitments(&self, mut rng: impl RngCore) -> Vec<RevealedCommitment> {
        self.inputs
            .iter()
            .map(|m| m.true_input.random_pseudo_commitment(&mut rng))
            .collect()
    }

    fn pseudo_commitments(
        &self,
        revealed_pseudo_commitments: &[RevealedCommitment],
    ) -> Vec<G1Affine> {
        revealed_pseudo_commitments
            .iter()
            .map(|r| r.commit(&Self::pc_gens()).to_affine())
            .collect()
    }

    fn revealed_output_commitments(
        &self,
        revealed_pseudo_commitments: &[RevealedCommitment],
        mut rng: impl RngCore,
    ) -> Vec<RevealedOutputCommitment> {
        // avoid subtraction underflow in next step.
        if self.outputs.is_empty() {
            return vec![];
        }

        let mut revealed_output_commitments: Vec<RevealedOutputCommitment> = self
            .outputs
            .iter()
            .map(|out| RevealedOutputCommitment {
                public_key: out.public_key,
                revealed_commitment: out.random_commitment(&mut rng),
            })
            .take(self.outputs.len() - 1)
            .collect();

        // todo: replace fold() with sum() when supported in blstrs
        let input_sum: Scalar = revealed_pseudo_commitments
            .iter()
            .map(RevealedCommitment::blinding)
            .fold(Scalar::zero(), |sum, x| sum + x);

        // todo: replace fold() with sum() when supported in blstrs
        let output_sum: Scalar = revealed_output_commitments
            .iter()
            .map(|r| r.revealed_commitment.blinding())
            .fold(Scalar::zero(), |sum, x| sum + x);

        let output_blinding_correction = input_sum - output_sum;

        if let Some(last_output) = self.outputs.last() {
            revealed_output_commitments.push(RevealedOutputCommitment {
                public_key: last_output.public_key,
                revealed_commitment: RevealedCommitment {
                    value: last_output.amount,
                    blinding: output_blinding_correction,
                },
            });
        } else {
            panic!("Expected at least one output")
        }
        revealed_output_commitments
    }

    fn output_range_proofs(
        &self,
        revealed_output_commitments: &[RevealedOutputCommitment],
        mut rng: impl RngCore + CryptoRng,
    ) -> Result<Vec<OutputProof>> {
        let mut prover_ts = Transcript::new(MERLIN_TRANSCRIPT_LABEL);

        let bp_gens = Self::bp_gens();

        revealed_output_commitments
            .iter()
            .map(|c| {
                let (range_proof, commitment) = RangeProof::prove_single_with_rng(
                    &bp_gens,
                    &Self::pc_gens(),
                    &mut prover_ts,
                    c.revealed_commitment.value,
                    &c.revealed_commitment.blinding,
                    RANGE_PROOF_BITS,
                    &mut rng,
                )?;

                Ok(OutputProof {
                    public_key: c.public_key,
                    range_proof,
                    commitment,
                })
            })
            .collect::<Result<Vec<_>>>()
    }
}

// note: used by both RingCtMaterial::sign and RingCtTransaction::verify()
//       which must match.
fn gen_message_for_signing(
    public_keys: &[G1Affine],
    key_images: &[G1Affine],
    pseudo_commitments: &[G1Affine],
    output_proofs: &[OutputProof],
) -> Vec<u8> {
    // Generate message to sign.
    let mut msg: Vec<u8> = Default::default();
    for pk in public_keys.iter() {
        msg.extend(pk.to_bytes().as_ref());
    }
    for t in key_images.iter() {
        msg.extend(t.to_bytes().as_ref());
    }
    for r in pseudo_commitments.iter() {
        msg.extend(r.to_bytes().as_ref());
    }
    for o in output_proofs.iter() {
        msg.extend(o.to_bytes());
    }
    msg
}

#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone)]
pub struct OutputProof {
    public_key: G1Affine,
    range_proof: RangeProof,
    commitment: G1Affine,
}

impl OutputProof {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut v: Vec<u8> = Default::default();
        v.extend(self.public_key.to_bytes().as_ref());
        v.extend(&self.range_proof.to_bytes());
        v.extend(self.commitment.to_bytes().as_ref());
        v
    }

    pub fn public_key(&self) -> &G1Affine {
        &self.public_key
    }

    pub fn range_proof(&self) -> &RangeProof {
        &self.range_proof
    }

    pub fn commitment(&self) -> G1Affine {
        self.commitment
    }
}

#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone)]
pub struct RingCtTransaction {
    pub mlsags: Vec<MlsagSignature>,
    pub outputs: Vec<OutputProof>,
}

impl RingCtTransaction {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut v: Vec<u8> = Default::default();
        for m in self.mlsags.iter() {
            v.extend(&m.to_bytes());
        }
        for o in self.outputs.iter() {
            v.extend(&o.to_bytes());
        }
        v
    }

    pub fn hash(&self) -> [u8; 32] {
        let mut sha3 = Sha3::v256();

        sha3.update(&self.to_bytes());

        let mut hash = [0; 32];
        sha3.finalize(&mut hash);
        hash
    }

    // note: must match message generated by RingCtMaterial::sign()
    pub fn gen_message(&self) -> Vec<u8> {
        // All public keys in all rings
        let public_keys: Vec<G1Affine> = self.mlsags.iter().flat_map(|m| m.public_keys()).collect();

        // All key-images (of true inputs),
        let key_images: Vec<G1Affine> = self.mlsags.iter().map(|m| m.key_image).collect();

        // All PseudoCommitments.
        let pseudo_commitments: Vec<G1Affine> =
            self.mlsags.iter().map(|m| m.pseudo_commitment()).collect();

        gen_message_for_signing(
            &public_keys,
            &key_images,
            &pseudo_commitments,
            &self.outputs,
        )
    }

    pub fn verify(&self, public_commitments_per_ring: &[Vec<G1Affine>]) -> Result<()> {
        let msg = self.gen_message();
        for (mlsag, public_commitments) in self.mlsags.iter().zip(public_commitments_per_ring) {
            mlsag.verify(&msg, public_commitments)?
        }

        let mut prover_ts = Transcript::new(MERLIN_TRANSCRIPT_LABEL);
        let bp_gens = RingCtMaterial::bp_gens();

        for output in self.outputs.iter() {
            // Verification requires a transcript with identical initial state:
            output.range_proof.verify_single(
                &bp_gens,
                &RingCtMaterial::pc_gens(),
                &mut prover_ts,
                &output.commitment,
                RANGE_PROOF_BITS,
            )?;
        }

        // Verify that the tx has at least one input
        if self.mlsags.is_empty() {
            return Err(Error::TransactionMustHaveAnInput);
        }

        // Verify that each KeyImage is unique in this tx.
        let keyimage_unique: BTreeSet<_> = self
            .mlsags
            .iter()
            .map(|m| m.key_image.to_compressed())
            .collect();
        if keyimage_unique.len() != self.mlsags.len() {
            return Err(Error::KeyImageNotUniqueAcrossInputs);
        }

        // Verify that each public_key is unique across all input mlsag
        let pk_unique: BTreeSet<_> = self
            .mlsags
            .iter()
            .flat_map(|m| {
                m.public_keys()
                    .iter()
                    .map(|pk| pk.to_compressed())
                    .collect::<Vec<[u8; 48]>>()
            })
            .collect();

        let pk_count = self.mlsags.iter().map(|m| m.public_keys().len()).sum();

        if pk_unique.len() != pk_count {
            return Err(Error::PublicKeyNotUniqueAcrossInputs);
        }

        let input_sum: G1Projective = self
            .mlsags
            .iter()
            .map(MlsagSignature::pseudo_commitment)
            .map(G1Projective::from)
            .sum();
        let output_sum: G1Projective = self
            .outputs
            .iter()
            .map(OutputProof::commitment)
            .map(G1Projective::from)
            .sum();

        if input_sum != output_sum {
            Err(Error::InputPseudoCommitmentsDoNotSumToOutputCommitments)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use bulletproofs::{
        group::{ff::Field, Curve, Group},
        rand::rngs::OsRng,
    };

    use crate::{DecoyInput, MlsagMaterial, TrueInput};

    use super::*;

    #[derive(Default)]
    struct TestLedger {
        commitments: BTreeMap<[u8; 48], G1Affine>, // Compressed public keys -> Commitments
    }

    impl TestLedger {
        fn log(&mut self, public_key: impl Into<G1Affine>, commitment: impl Into<G1Affine>) {
            self.commitments
                .insert(public_key.into().to_compressed(), commitment.into());
        }

        fn lookup(&self, public_key: impl Into<G1Affine>) -> Option<G1Affine> {
            self.commitments
                .get(&public_key.into().to_compressed())
                .copied()
        }

        fn fetch_decoys(&self, n: usize, exclude: &[G1Projective]) -> Vec<DecoyInput> {
            let exclude_set = BTreeSet::from_iter(exclude.iter().map(G1Projective::to_compressed));

            self.commitments
                .iter()
                .filter(|(pk, _)| !exclude_set.contains(*pk))
                .map(|(pk, c)| DecoyInput {
                    public_key: G1Affine::from_compressed(pk).unwrap(),
                    commitment: *c,
                })
                .take(n)
                .collect()
        }
    }

    #[test]
    fn test_ringct_sign() {
        let mut rng = OsRng::default();
        let pc_gens = PedersenGens::default();

        let true_input = TrueInput {
            secret_key: Scalar::random(&mut rng),
            revealed_commitment: RevealedCommitment {
                value: 3,
                blinding: 5.into(),
            },
        };

        let mut ledger = TestLedger::default();
        ledger.log(
            true_input.public_key(),
            true_input.revealed_commitment.commit(&pc_gens),
        );
        ledger.log(
            G1Projective::random(&mut rng),
            G1Projective::random(&mut rng),
        );
        ledger.log(
            G1Projective::random(&mut rng),
            G1Projective::random(&mut rng),
        );

        let decoy_inputs = ledger.fetch_decoys(2, &[true_input.public_key()]);

        let ring_ct = RingCtMaterial {
            inputs: vec![MlsagMaterial::new(true_input, decoy_inputs, &mut rng)],
            outputs: vec![Output {
                public_key: G1Projective::random(&mut rng).to_affine(),
                amount: 3,
            }],
        };

        let (signed_tx, _revealed_output_commitments) =
            ring_ct.sign(rng).expect("Failed to sign transaction");

        let public_commitments = Vec::from_iter(signed_tx.mlsags.iter().map(|mlsag| {
            Vec::from_iter(
                mlsag
                    .public_keys()
                    .into_iter()
                    .map(|pk| ledger.lookup(pk).unwrap()),
            )
        }));

        assert!(signed_tx.verify(&public_commitments).is_ok());
    }
}
