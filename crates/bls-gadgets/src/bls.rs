use crate::Bitmap;
use algebra::{PairingEngine, PrimeField, ProjectiveCurve};
use r1cs_core::{SynthesisError, ConstraintSystemRef};
use r1cs_std::{
    boolean::Boolean, eq::EqGadget, fields::fp::FpVar, fields::FieldVar, R1CSVar,
    groups::CurveVar, pairing::PairingVar, alloc::AllocVar,
};
use std::marker::PhantomData;
use std::ops::AddAssign;
use tracing::{debug, span, trace, Level};

/// BLS Signature Verification Gadget.
///
/// Implements BLS Verification as written in [BDN18](https://eprint.iacr.org/2018/483.pdf)
/// in a Pairing-based SNARK.
pub struct BlsVerifyGadget<E, F, P> {
    /// The curve being used
    pairing_engine_type: PhantomData<E>,
    /// The field we're operating on
    constraint_field_type: PhantomData<F>,
    /// The pairing gadget we use, which MUST match our pairing engine
    pairing_gadget_type: PhantomData<P>,
}

impl<E, F, P> BlsVerifyGadget<E, F, P>
where
    E: PairingEngine,
    F: PrimeField,
    P: PairingVar<E, F>,
    P::G2Var: for<'a> AddAssign<&'a P::G2Var>,
{
    /// Enforces verification of a BLS Signature against a list of public keys and a bitmap indicating
    /// which of these pubkeys signed.
    ///
    /// A maximum number of non_signers is also provided to
    /// indicate our threshold
    ///
    /// The verification equation can be found in pg.11 from
    /// https://eprint.iacr.org/2018/483.pdf: "Multi-Signature Verification"
    pub fn verify(
        pub_keys: &[P::G2Var],
        signed_bitmap: &[Boolean<F>],
        message_hash: &P::G1Var,
        signature: &P::G1Var,
        maximum_non_signers: &FpVar<F>,
    ) -> Result<(), SynthesisError> {
        let span = span!(Level::TRACE, "BlsVerifyGadget_verify");
        let _enter = span.enter();
        // Get the message hash and the aggregated public key based on the bitmap
        // and allowed number of non-signers
        let (message_hash, aggregated_pk) = Self::enforce_bitmap(
            pub_keys,
            signed_bitmap,
            message_hash,
            maximum_non_signers,
        )?;

        let prepared_aggregated_pk =
            P::prepare_g2(&aggregated_pk)?;

        let prepared_message_hash =
            P::prepare_g1(&message_hash)?;

        // Prepare the signature and get the generator
        let (prepared_signature, prepared_g2_neg_generator) =
            Self::prepare_signature_neg_generator(&signature)?;

        // e(σ, g_2^-1) * e(H(m), apk) == 1_{G_T}
        Self::enforce_bls_equation(
            &[prepared_signature, prepared_message_hash],
            &[prepared_g2_neg_generator, prepared_aggregated_pk],
        )?;

        Ok(())
    }

    /// Enforces batch verification of a an aggregate BLS Signature against a
    /// list of (pubkey, message) tuples.
    ///
    /// The verification equation can be found in pg.11 from
    /// https://eprint.iacr.org/2018/483.pdf: "Batch verification"
    pub fn batch_verify(
        aggregated_pub_keys: &[P::G2Var],
        message_hashes: &[P::G1Var],
        aggregated_signature: &P::G1Var,
    ) -> Result<(), SynthesisError> {
        debug!("batch verifying BLS signature");
        let prepared_message_hashes = message_hashes
            .iter()
            .enumerate()
            .map(|(_i, message_hash)| {
                P::prepare_g1(
                    &message_hash,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        let prepared_aggregated_pub_keys = aggregated_pub_keys
            .iter()
            .enumerate()
            .map(|(_i, pubkey)| P::prepare_g2(&pubkey))
            .collect::<Result<Vec<_>, _>>()?;

        Self::batch_verify_prepared(
            &prepared_aggregated_pub_keys,
            &prepared_message_hashes,
            aggregated_signature,
        )
    }

    /// Batch verification against prepared messages
    pub fn batch_verify_prepared(
        prepared_aggregated_pub_keys: &[P::G2PreparedVar],
        prepared_message_hashes: &[P::G1PreparedVar],
        aggregated_signature: &P::G1Var,
    ) -> Result<(), SynthesisError> {
        // Prepare the signature and get the generator
        let (prepared_signature, prepared_g2_neg_generator) =
            Self::prepare_signature_neg_generator(aggregated_signature)?;

        // Create the vectors which we'll batch verify
        let mut prepared_g1s = vec![prepared_signature];
        let mut prepared_g2s = vec![prepared_g2_neg_generator];
        prepared_g1s.extend_from_slice(&prepared_message_hashes);
        prepared_g2s.extend_from_slice(&prepared_aggregated_pub_keys);

        // Enforce the BLS check
        // e(σ, g_2^-1) * e(H(m0), pk_0) * e(H(m1), pk_1) ...  * e(H(m_n), pk_n)) == 1_{G_T}
        Self::enforce_bls_equation(&prepared_g1s, &prepared_g2s)?;

        Ok(())
    }

    /// Returns a gadget which checks that an aggregate pubkey is correctly calculated
    /// by the sum of the pub keys which had a 1 in the bitmap
    ///
    /// # Panics
    /// If signed_bitmap length != pub_keys length
    pub fn enforce_aggregated_pubkeys(
        pub_keys: &[P::G2Var],
        signed_bitmap: &[Boolean<F>],
    ) -> Result<P::G2Var, SynthesisError> {
        // Bitmap and Pubkeys must be of the same length
        assert_eq!(signed_bitmap.len(), pub_keys.len());

        let mut aggregated_pk = P::G2Var::zero();
        for (_i, (pk, bit)) in pub_keys.iter().zip(signed_bitmap).enumerate() {
            // If bit = 1, add pk
            let adder = bit.select(pk, &P::G2Var::zero())?;
            aggregated_pk += &adder;
        }

        Ok(aggregated_pk)
    }

    /// Returns a gadget which checks that an aggregate pubkey is correctly calculated
    /// by the sum of the pub keys
    pub fn enforce_aggregated_all_pubkeys(
        pub_keys: &[P::G2Var],
    ) -> Result<P::G2Var, SynthesisError> {
        let mut aggregated_pk = P::G2Var::zero();
        for (_i, pk) in pub_keys.iter().enumerate() {
            // Add the pubkey to the sum
            // aggregated_pk += pk
            aggregated_pk += pk; 
        }

        Ok(aggregated_pk)
    }

    /// Enforces that the provided bitmap contains no more than `maximum_non_signers`
    /// 0s. Also returns a gadget of the prepared message hash and a gadget for the aggregate public key
    ///
    /// # Panics
    /// If signed_bitmap length != pub_keys length (due to internal call to `enforced_aggregated_pubkeys`)
    pub fn enforce_bitmap(
        pub_keys: &[P::G2Var],
        signed_bitmap: &[Boolean<F>],
        message_hash: &P::G1Var,
        maximum_non_signers: &FpVar<F>,
    ) -> Result<(P::G1Var, P::G2Var), SynthesisError> {
        trace!("enforcing bitmap");
        signed_bitmap.enforce_maximum_occurrences_in_bitmap(maximum_non_signers, false)?;

        let aggregated_pk = Self::enforce_aggregated_pubkeys(pub_keys, signed_bitmap)?;

        Ok((message_hash.clone(), aggregated_pk))
    }

    /// Verifying BLS signatures requires preparing a G1 Signature and
    /// preparing a negated G2 generator
    fn prepare_signature_neg_generator(
        signature: &P::G1Var,
    ) -> Result<(P::G1PreparedVar, P::G2PreparedVar), SynthesisError> {
        // Ensure the signature is prepared
        let prepared_signature = P::prepare_g1(signature)?;

        // Allocate the generator on G2
        let g2_generator = <P::G2Var as AllocVar<E::G2Projective,F>>::new_constant(
            signature.cs().unwrap_or(ConstraintSystemRef::None),
            E::G2Projective::prime_subgroup_generator(),
        )?;
        // and negate it for the purpose of verification
        let g2_neg_generator = g2_generator.negate()?;
        let prepared_g2_neg_generator =
            P::prepare_g2(&g2_neg_generator)?;

        Ok((prepared_signature, prepared_g2_neg_generator))
    }

    /// Multiply the pairings together and check that their product == 1 in G_T, which indicates
    /// that the verification has passed.
    ///
    /// Each G1 element is paired with the corresponding G2 element.
    /// Fails if the 2 slices have different lengths.
    fn enforce_bls_equation(
        g1: &[P::G1PreparedVar],
        g2: &[P::G2PreparedVar],
    ) -> Result<(), SynthesisError> {
        trace!("enforcing BLS equation");
        let bls_equation = P::product_of_pairings(g1, g2)?;
        let gt_one = &P::GTVar::one();
        bls_equation.enforce_equal(gt_one)?;
        Ok(())
    }
}

#[cfg(test)]
mod verify_one_message {
    use super::*;
//    use crate::utils::test_helpers::alloc_vec;
    use bls_crypto::test_helpers::*;

    use algebra::{
        bls12_377::{Bls12_377, Fr as Bls12_377Fr, G1Projective, G2Projective, Parameters as Bls12_377_Parameters},
        bw6_761::Fr as BW6_761Fr,
        ProjectiveCurve, UniformRand, Zero,
    };
    use r1cs_core::ConstraintSystem;
    use r1cs_std::{
        alloc::AllocVar,
        bls12_377::{G1Var, G2Var, PairingVar as Bls12_377PairingGadget},
        boolean::Boolean,
//        test_constraint_system::TestConstraintSystem,
    };

    // converts the arguments to constraints and checks them against the `verify` function
    fn cs_verify<E: PairingEngine, F: PrimeField, P: PairingVar<E, F>>(
        message_hash: E::G1Projective,
        pub_keys: &[E::G2Projective],
        signature: E::G1Projective,
        bitmap: &[bool],
        num_non_signers: u64,
    ) -> ConstraintSystemRef<F> {
        let mut cs = ConstraintSystem::<F>::new_ref();

        let message_hash_var =
            <P::G1Var as AllocVar<E::G1Projective, _>>::new_witness(cs.clone(), || Ok(message_hash)).unwrap();
        let signature_var = <P::G1Var as AllocVar<E::G1Projective, _>>::new_witness(cs.clone(), || Ok(signature)).unwrap();

        let pub_keys = pub_keys
            .iter()
            .enumerate()
            .map(|(i, pub_key)| {
                <P::G2Var as AllocVar<E::G2Projective, _>>::new_witness(cs.clone(), || Ok(pub_key)).unwrap()
            })
            .collect::<Vec<_>>();
        let bitmap = bitmap
            .iter()
            .map(|b| Boolean::new_witness(cs.clone(), || Ok(*b)).unwrap())
            .collect::<Vec<_>>();

        let max_occurrences =
            &FpVar::<F>::new_witness(cs.clone(), || Ok(F::from(num_non_signers)))
                .unwrap();
        BlsVerifyGadget::<E, F, P>::verify(
            &pub_keys,
            &bitmap[..],
            &message_hash_var,
            &signature_var,
            &max_occurrences,
        )
        .unwrap();

        cs
    }

    #[test]
    fn batch_verify_ok() {
        // generate 5 (aggregate sigs, message hash pairs)
        // verify them all in 1 call
        let batch_size = 5;
        let num_keys = 7;
        let rng = &mut rand::thread_rng();

        // generate some random messages
        let messages = (0..batch_size)
            .map(|_| G1Projective::rand(rng))
            .collect::<Vec<_>>();
        // keygen for multiple rounds (7 keys per round)
        let (secret_keys, public_keys_batches) = keygen_batch::<Bls12_377>(batch_size, num_keys);
        // get the aggregate public key for each rounds
        let aggregate_pubkeys = public_keys_batches
            .iter()
            .map(|pks| sum(pks))
            .collect::<Vec<_>>();
        // the keys from each epoch sign the messages from the corresponding epoch
        let asigs = sign_batch::<Bls12_377>(&secret_keys, &messages);
        // get the complete aggregate signature
        let asig = sum(&asigs);

        // allocate the constraints
        let mut cs = ConstraintSystem::<BW6_761Fr>::new_ref();
        let messages = messages.iter().enumerate().map(|(i, element)| <G1Var as AllocVar<G1Projective, _>>::new_witness(cs.clone(), || Ok(element)).unwrap()).collect::<Vec<_>>(); //alloc_vec(cs.clone(), &messages);
        let aggregate_pubkeys = aggregate_pubkeys.iter().enumerate().map(|(i, element)| <G2Var as AllocVar<G2Projective, _>>::new_witness(cs.clone(), || Ok(element)).unwrap()).collect::<Vec<_>>(); //alloc_vec(cs.clone(), &aggregate_pubkeys);
        let asig = G1Var::new_witness(cs.clone(), || Ok(asig)).unwrap();

        // check that verification is correct
        BlsVerifyGadget::<Bls12_377, BW6_761Fr, Bls12_377PairingGadget>::batch_verify(
            &aggregate_pubkeys,
            &messages,
            &asig,
        )
        .unwrap();
        assert!(cs.is_satisfied().unwrap());
    }

    #[test]
    // Verifies signatures over BLS12_377 with Sw6 field (384 bits).
    fn one_signature_ok() {
        let (secret_key, pub_key) = keygen::<Bls12_377>();
        let rng = &mut rng();
        let message_hash = G1Projective::rand(rng);
        let signature = message_hash.mul(secret_key);
        let fake_signature = G1Projective::rand(rng);

        // good sig passes
        let cs = cs_verify::<Bls12_377, BW6_761Fr, Bls12_377PairingGadget>(
            message_hash,
            &[pub_key],
            signature,
            &[true],
            0,
        );
        assert!(cs.is_satisfied().unwrap());
        //assert_eq!(cs.num_constraints(), 21184);

        // random sig fails
        let cs = cs_verify::<Bls12_377, BW6_761Fr, Bls12_377PairingGadget>(
            message_hash,
            &[pub_key],
            fake_signature,
            &[true],
            0,
        );
        assert!(!cs.is_satisfied().unwrap());
    }

    #[test]
    fn multiple_signatures_ok() {
        let rng = &mut rng();
        let message_hash = G1Projective::rand(rng);
        let (sk, pk) = keygen::<Bls12_377>();
        let (sk2, pk2) = keygen::<Bls12_377>();
        let (sigs, asig) = sign::<Bls12_377>(message_hash, &[sk, sk2]);

        // good aggregate sig passes
        let cs = cs_verify::<Bls12_377, BW6_761Fr, Bls12_377PairingGadget>(
            message_hash,
            &[pk, pk2],
            asig,
            &[true, true],
            1,
        );
        assert!(cs.is_satisfied().unwrap());

        // using the single sig if second guy is OK as long as
        // we tolerate 1 non-signers
        let cs = cs_verify::<Bls12_377, BW6_761Fr, Bls12_377PairingGadget>(
            message_hash,
            &[pk, pk2],
            sigs[0],
            &[true, false],
            1,
        );
        assert!(cs.is_satisfied().unwrap());

        // bitmap set to false on the second one fails since we don't tolerate
        // >0 failures
        let cs = cs_verify::<Bls12_377, BW6_761Fr, Bls12_377PairingGadget>(
            message_hash,
            &[pk, pk2],
            asig,
            &[true, false],
            0,
        );
        assert!(!cs.is_satisfied().unwrap());
        let cs = cs_verify::<Bls12_377, BW6_761Fr, Bls12_377PairingGadget>(
            message_hash,
            &[pk, pk2],
            sigs[0],
            &[true, false],
            0,
        );
        assert!(!cs.is_satisfied().unwrap());
    }

    #[test]
    fn zero_fails() {
        let rng = &mut rng();
        let message_hash = G1Projective::rand(rng);
        let generator = G2Projective::prime_subgroup_generator();

        // if the first key is a bad one, it should fail, since the pubkey
        // won't be on the curve
        let sk = Bls12_377Fr::zero();
        let pk = generator.clone().mul(sk);
        let (sk2, pk2) = keygen::<Bls12_377>();

        let (sigs, _) = sign::<Bls12_377>(message_hash, &[sk, sk2]);

        let cs = cs_verify::<Bls12_377, BW6_761Fr, Bls12_377PairingGadget>(
            message_hash,
            &[pk, pk2],
            sigs[1],
            &[false, true],
            3,
        );
        assert!(!cs.is_satisfied().unwrap());
    }
}
