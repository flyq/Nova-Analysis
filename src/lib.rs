//! This library implements Nova, a high-speed recursive SNARK.
#![deny(
  warnings,
  unused,
  future_incompatible,
  nonstandard_style,
  rust_2018_idioms,
  missing_docs
)]
#![allow(non_snake_case)]
#![allow(clippy::type_complexity)]
#![forbid(unsafe_code)]

pub mod bellpepper;
pub mod circuit;
pub mod constants;
pub mod nifs;
pub mod r1cs;

pub mod errors;
pub mod gadgets;
pub mod provider;
pub mod spartan;
pub mod traits;

use crate::bellpepper::{
  r1cs::{NovaShape, NovaWitness},
  shape_cs::ShapeCS,
  solver::SatisfyingAssignment,
};
use bellpepper_core::ConstraintSystem;
use circuit::{NovaAugmentedCircuit, NovaAugmentedCircuitInputs, NovaAugmentedCircuitParams};
use constants::{BN_LIMB_WIDTH, BN_N_LIMBS, NUM_FE_WITHOUT_IO_FOR_CRHF, NUM_HASH_BITS};
use core::marker::PhantomData;
use errors::NovaError;
use ff::Field;
use gadgets::utils::scalar_as_base;
use nifs::NIFS;
use r1cs::{R1CSInstance, R1CSShape, R1CSWitness, RelaxedR1CSInstance, RelaxedR1CSWitness};
use serde::{Deserialize, Serialize};
use sha3::{Digest, Sha3_256};
use traits::{
  circuit::StepCircuit,
  commitment::{CommitmentEngineTrait, CommitmentTrait},
  snark::RelaxedR1CSSNARKTrait,
  AbsorbInROTrait, Group, ROConstants, ROConstantsCircuit, ROTrait,
};

/// A type that holds public parameters of Nova
#[derive(Serialize, Deserialize)]
#[serde(bound = "")]
pub struct PublicParams<G1, G2, C1, C2>
where
  G1: Group<Base = <G2 as Group>::Scalar>,
  G2: Group<Base = <G1 as Group>::Scalar>,
  C1: StepCircuit<G1::Scalar>,
  C2: StepCircuit<G2::Scalar>,
{
  F_arity_primary: usize,
  F_arity_secondary: usize,
  ro_consts_primary: ROConstants<G1>,
  ro_consts_circuit_primary: ROConstantsCircuit<G2>,
  ck_primary: CommitmentKey<G1>,
  r1cs_shape_primary: R1CSShape<G1>,
  ro_consts_secondary: ROConstants<G2>,
  ro_consts_circuit_secondary: ROConstantsCircuit<G1>,
  ck_secondary: CommitmentKey<G2>,
  r1cs_shape_secondary: R1CSShape<G2>,
  augmented_circuit_params_primary: NovaAugmentedCircuitParams,
  augmented_circuit_params_secondary: NovaAugmentedCircuitParams,
  digest: G1::Scalar, // digest of everything else with this field set to G1::Scalar::ZERO
  _p_c1: PhantomData<C1>,
  _p_c2: PhantomData<C2>,
}

impl<G1, G2, C1, C2> PublicParams<G1, G2, C1, C2>
where
  G1: Group<Base = <G2 as Group>::Scalar>,
  G2: Group<Base = <G1 as Group>::Scalar>,
  C1: StepCircuit<G1::Scalar>,
  C2: StepCircuit<G2::Scalar>,
{
  /// Create a new `PublicParams`
  pub fn setup(c_primary: &C1, c_secondary: &C2) -> Self {
    let augmented_circuit_params_primary =
      NovaAugmentedCircuitParams::new(BN_LIMB_WIDTH, BN_N_LIMBS, true);
    let augmented_circuit_params_secondary =
      NovaAugmentedCircuitParams::new(BN_LIMB_WIDTH, BN_N_LIMBS, false);

    let ro_consts_primary: ROConstants<G1> = ROConstants::<G1>::default();
    let ro_consts_secondary: ROConstants<G2> = ROConstants::<G2>::default();

    let F_arity_primary = c_primary.arity();
    let F_arity_secondary = c_secondary.arity();

    // ro_consts_circuit_primary are parameterized by G2 because the type alias uses G2::Base = G1::Scalar
    let ro_consts_circuit_primary: ROConstantsCircuit<G2> = ROConstantsCircuit::<G2>::default();
    let ro_consts_circuit_secondary: ROConstantsCircuit<G1> = ROConstantsCircuit::<G1>::default();

    // Initialize ck for the primary
    let circuit_primary: NovaAugmentedCircuit<'_, G2, C1> = NovaAugmentedCircuit::new(
      &augmented_circuit_params_primary,
      None,
      c_primary,
      ro_consts_circuit_primary.clone(),
    );
    let mut cs: ShapeCS<G1> = ShapeCS::new();
    let _ = circuit_primary.synthesize(&mut cs);
    let (r1cs_shape_primary, ck_primary) = cs.r1cs_shape();

    // Initialize ck for the secondary
    let circuit_secondary: NovaAugmentedCircuit<'_, G1, C2> = NovaAugmentedCircuit::new(
      &augmented_circuit_params_secondary,
      None,
      c_secondary,
      ro_consts_circuit_secondary.clone(),
    );
    let mut cs: ShapeCS<G2> = ShapeCS::new();
    let _ = circuit_secondary.synthesize(&mut cs);
    let (r1cs_shape_secondary, ck_secondary) = cs.r1cs_shape();

    let mut pp = Self {
      F_arity_primary,
      F_arity_secondary,
      ro_consts_primary,
      ro_consts_circuit_primary,
      ck_primary,
      r1cs_shape_primary,
      ro_consts_secondary,
      ro_consts_circuit_secondary,
      ck_secondary,
      r1cs_shape_secondary,
      augmented_circuit_params_primary,
      augmented_circuit_params_secondary,
      digest: G1::Scalar::ZERO,
      _p_c1: Default::default(),
      _p_c2: Default::default(),
    };

    // set the digest in pp
    pp.digest = compute_digest::<G1, PublicParams<G1, G2, C1, C2>>(&pp);

    pp
  }

  /// Returns the number of constraints in the primary and secondary circuits
  pub const fn num_constraints(&self) -> (usize, usize) {
    (
      self.r1cs_shape_primary.num_cons,
      self.r1cs_shape_secondary.num_cons,
    )
  }

  /// Returns the number of variables in the primary and secondary circuits
  pub const fn num_variables(&self) -> (usize, usize) {
    (
      self.r1cs_shape_primary.num_vars,
      self.r1cs_shape_secondary.num_vars,
    )
  }
}

/// A SNARK that proves the correct execution of an incremental computation
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(bound = "")]
pub struct RecursiveSNARK<G1, G2, C1, C2>
where
  G1: Group<Base = <G2 as Group>::Scalar>,
  G2: Group<Base = <G1 as Group>::Scalar>,
  C1: StepCircuit<G1::Scalar>,
  C2: StepCircuit<G2::Scalar>,
{
  r_W_primary: RelaxedR1CSWitness<G1>,
  r_U_primary: RelaxedR1CSInstance<G1>,
  r_W_secondary: RelaxedR1CSWitness<G2>,
  r_U_secondary: RelaxedR1CSInstance<G2>,
  l_w_secondary: R1CSWitness<G2>,
  l_u_secondary: R1CSInstance<G2>,
  i: usize,
  zi_primary: Vec<G1::Scalar>,
  zi_secondary: Vec<G2::Scalar>,
  _p_c1: PhantomData<C1>,
  _p_c2: PhantomData<C2>,
}

impl<G1, G2, C1, C2> RecursiveSNARK<G1, G2, C1, C2>
where
  G1: Group<Base = <G2 as Group>::Scalar>,
  G2: Group<Base = <G1 as Group>::Scalar>,
  C1: StepCircuit<G1::Scalar>,
  C2: StepCircuit<G2::Scalar>,
{
  /// Create new instance of recursive SNARK
  pub fn new(
    pp: &PublicParams<G1, G2, C1, C2>,
    c_primary: &C1,
    c_secondary: &C2,
    z0_primary: Vec<G1::Scalar>,
    z0_secondary: Vec<G2::Scalar>,
  ) -> Self {
    // base case for the primary
    let mut cs_primary: SatisfyingAssignment<G1> = SatisfyingAssignment::new();
    let inputs_primary: NovaAugmentedCircuitInputs<G2> = NovaAugmentedCircuitInputs::new(
      scalar_as_base::<G1>(pp.digest),
      G1::Scalar::ZERO,
      z0_primary,
      None,
      None,
      None,
      None,
    );

    let circuit_primary: NovaAugmentedCircuit<'_, G2, C1> = NovaAugmentedCircuit::new(
      &pp.augmented_circuit_params_primary,
      Some(inputs_primary),
      c_primary,
      pp.ro_consts_circuit_primary.clone(),
    );
    let zi_primary = circuit_primary
      .synthesize(&mut cs_primary)
      .map_err(|_| NovaError::SynthesisError)
      .expect("Nova error synthesis");
    let (u_primary, w_primary) = cs_primary
      .r1cs_instance_and_witness(&pp.r1cs_shape_primary, &pp.ck_primary)
      .map_err(|_e| NovaError::UnSat)
      .expect("Nova error unsat");

    // base case for the secondary
    let mut cs_secondary: SatisfyingAssignment<G2> = SatisfyingAssignment::new();
    let inputs_secondary: NovaAugmentedCircuitInputs<G1> = NovaAugmentedCircuitInputs::new(
      pp.digest,
      G2::Scalar::ZERO,
      z0_secondary,
      None,
      None,
      Some(u_primary.clone()),
      None,
    );
    let circuit_secondary: NovaAugmentedCircuit<'_, G1, C2> = NovaAugmentedCircuit::new(
      &pp.augmented_circuit_params_secondary,
      Some(inputs_secondary),
      c_secondary,
      pp.ro_consts_circuit_secondary.clone(),
    );
    let zi_secondary = circuit_secondary
      .synthesize(&mut cs_secondary)
      .map_err(|_| NovaError::SynthesisError)
      .expect("Nova error synthesis");
    let (u_secondary, w_secondary) = cs_secondary
      .r1cs_instance_and_witness(&pp.r1cs_shape_secondary, &pp.ck_secondary)
      .map_err(|_e| NovaError::UnSat)
      .expect("Nova error unsat");

    // IVC proof for the primary circuit
    let l_w_primary = w_primary;
    let l_u_primary = u_primary;
    let r_W_primary = RelaxedR1CSWitness::from_r1cs_witness(&pp.r1cs_shape_primary, &l_w_primary);
    let r_U_primary =
      RelaxedR1CSInstance::from_r1cs_instance(&pp.ck_primary, &pp.r1cs_shape_primary, &l_u_primary);

    // IVC proof of the secondary circuit
    let l_w_secondary = w_secondary;
    let l_u_secondary = u_secondary;
    let r_W_secondary = RelaxedR1CSWitness::<G2>::default(&pp.r1cs_shape_secondary);
    let r_U_secondary =
      RelaxedR1CSInstance::<G2>::default(&pp.ck_secondary, &pp.r1cs_shape_secondary);

    if zi_primary.len() != pp.F_arity_primary || zi_secondary.len() != pp.F_arity_secondary {
      panic!("Invalid step length");
    }

    let zi_primary = zi_primary
      .iter()
      .map(|v| v.get_value().ok_or(NovaError::SynthesisError))
      .collect::<Result<Vec<<G1 as Group>::Scalar>, NovaError>>()
      .expect("Nova error synthesis");

    let zi_secondary = zi_secondary
      .iter()
      .map(|v| v.get_value().ok_or(NovaError::SynthesisError))
      .collect::<Result<Vec<<G2 as Group>::Scalar>, NovaError>>()
      .expect("Nova error synthesis");

    Self {
      r_W_primary,
      r_U_primary,
      r_W_secondary,
      r_U_secondary,
      l_w_secondary,
      l_u_secondary,
      i: 0,
      zi_primary,
      zi_secondary,
      _p_c1: Default::default(),
      _p_c2: Default::default(),
    }
  }

  /// Create a new `RecursiveSNARK` (or updates the provided `RecursiveSNARK`)
  /// by executing a step of the incremental computation
  pub fn prove_step(
    &mut self,
    pp: &PublicParams<G1, G2, C1, C2>,
    c_primary: &C1,
    c_secondary: &C2,
    z0_primary: Vec<G1::Scalar>,
    z0_secondary: Vec<G2::Scalar>,
  ) -> Result<(), NovaError> {
    if z0_primary.len() != pp.F_arity_primary || z0_secondary.len() != pp.F_arity_secondary {
      return Err(NovaError::InvalidInitialInputLength);
    }

    // Frist step was already done in the constructor
    if self.i == 0 {
      self.i = 1;
      return Ok(());
    }

    // fold the secondary circuit's instance
    let (nifs_secondary, (r_U_secondary, r_W_secondary)) = NIFS::prove(
      &pp.ck_secondary,
      &pp.ro_consts_secondary,
      &scalar_as_base::<G1>(pp.digest),
      &pp.r1cs_shape_secondary,
      &self.r_U_secondary,
      &self.r_W_secondary,
      &self.l_u_secondary,
      &self.l_w_secondary,
    )
    .expect("Unable to fold secondary");

    let mut cs_primary: SatisfyingAssignment<G1> = SatisfyingAssignment::new();
    let inputs_primary: NovaAugmentedCircuitInputs<G2> = NovaAugmentedCircuitInputs::new(
      scalar_as_base::<G1>(pp.digest),
      G1::Scalar::from(self.i as u64),
      z0_primary,
      Some(self.zi_primary.clone()),
      Some(self.r_U_secondary.clone()),
      Some(self.l_u_secondary.clone()),
      Some(Commitment::<G2>::decompress(&nifs_secondary.comm_T)?),
    );

    let circuit_primary: NovaAugmentedCircuit<'_, G2, C1> = NovaAugmentedCircuit::new(
      &pp.augmented_circuit_params_primary,
      Some(inputs_primary),
      c_primary,
      pp.ro_consts_circuit_primary.clone(),
    );
    let zi_primary = circuit_primary
      .synthesize(&mut cs_primary)
      .map_err(|_| NovaError::SynthesisError)?;

    let (l_u_primary, l_w_primary) = cs_primary
      .r1cs_instance_and_witness(&pp.r1cs_shape_primary, &pp.ck_primary)
      .map_err(|_e| NovaError::UnSat)
      .expect("Nova error unsat");

    // fold the primary circuit's instance
    let (nifs_primary, (r_U_primary, r_W_primary)) = NIFS::prove(
      &pp.ck_primary,
      &pp.ro_consts_primary,
      &pp.digest,
      &pp.r1cs_shape_primary,
      &self.r_U_primary,
      &self.r_W_primary,
      &l_u_primary,
      &l_w_primary,
    )
    .expect("Unable to fold primary");

    let mut cs_secondary: SatisfyingAssignment<G2> = SatisfyingAssignment::new();
    let inputs_secondary: NovaAugmentedCircuitInputs<G1> = NovaAugmentedCircuitInputs::new(
      pp.digest,
      G2::Scalar::from(self.i as u64),
      z0_secondary,
      Some(self.zi_secondary.clone()),
      Some(self.r_U_primary.clone()),
      Some(l_u_primary),
      Some(Commitment::<G1>::decompress(&nifs_primary.comm_T)?),
    );

    let circuit_secondary: NovaAugmentedCircuit<'_, G1, C2> = NovaAugmentedCircuit::new(
      &pp.augmented_circuit_params_secondary,
      Some(inputs_secondary),
      c_secondary,
      pp.ro_consts_circuit_secondary.clone(),
    );
    let zi_secondary = circuit_secondary
      .synthesize(&mut cs_secondary)
      .map_err(|_| NovaError::SynthesisError)?;

    let (l_u_secondary, l_w_secondary) = cs_secondary
      .r1cs_instance_and_witness(&pp.r1cs_shape_secondary, &pp.ck_secondary)
      .map_err(|_e| NovaError::UnSat)?;

    // update the running instances and witnesses
    self.zi_primary = zi_primary
      .iter()
      .map(|v| v.get_value().ok_or(NovaError::SynthesisError))
      .collect::<Result<Vec<<G1 as Group>::Scalar>, NovaError>>()?;
    self.zi_secondary = zi_secondary
      .iter()
      .map(|v| v.get_value().ok_or(NovaError::SynthesisError))
      .collect::<Result<Vec<<G2 as Group>::Scalar>, NovaError>>()?;

    self.l_u_secondary = l_u_secondary;
    self.l_w_secondary = l_w_secondary;

    self.r_U_primary = r_U_primary;
    self.r_W_primary = r_W_primary;

    self.i += 1;

    self.r_U_secondary = r_U_secondary;
    self.r_W_secondary = r_W_secondary;

    Ok(())
  }

  /// Verify the correctness of the `RecursiveSNARK`
  pub fn verify(
    &self,
    pp: &PublicParams<G1, G2, C1, C2>,
    num_steps: usize,
    z0_primary: &[G1::Scalar],
    z0_secondary: &[G2::Scalar],
  ) -> Result<(Vec<G1::Scalar>, Vec<G2::Scalar>), NovaError> {
    // number of steps cannot be zero
    if num_steps == 0 {
      return Err(NovaError::ProofVerifyError);
    }

    // check if the provided proof has executed num_steps
    if self.i != num_steps {
      return Err(NovaError::ProofVerifyError);
    }

    // check if the (relaxed) R1CS instances have two public outputs
    if self.l_u_secondary.X.len() != 2
      || self.r_U_primary.X.len() != 2
      || self.r_U_secondary.X.len() != 2
    {
      return Err(NovaError::ProofVerifyError);
    }

    // check if the output hashes in R1CS instances point to the right running instances
    let (hash_primary, hash_secondary) = {
      let mut hasher = <G2 as Group>::RO::new(
        pp.ro_consts_secondary.clone(),
        NUM_FE_WITHOUT_IO_FOR_CRHF + 2 * pp.F_arity_primary,
      );
      hasher.absorb(pp.digest);
      hasher.absorb(G1::Scalar::from(num_steps as u64));
      for e in z0_primary {
        hasher.absorb(*e);
      }
      for e in &self.zi_primary {
        hasher.absorb(*e);
      }
      self.r_U_secondary.absorb_in_ro(&mut hasher);

      let mut hasher2 = <G1 as Group>::RO::new(
        pp.ro_consts_primary.clone(),
        NUM_FE_WITHOUT_IO_FOR_CRHF + 2 * pp.F_arity_secondary,
      );
      hasher2.absorb(scalar_as_base::<G1>(pp.digest));
      hasher2.absorb(G2::Scalar::from(num_steps as u64));
      for e in z0_secondary {
        hasher2.absorb(*e);
      }
      for e in &self.zi_secondary {
        hasher2.absorb(*e);
      }
      self.r_U_primary.absorb_in_ro(&mut hasher2);

      (
        hasher.squeeze(NUM_HASH_BITS),
        hasher2.squeeze(NUM_HASH_BITS),
      )
    };

    if hash_primary != self.l_u_secondary.X[0]
      || hash_secondary != scalar_as_base::<G2>(self.l_u_secondary.X[1])
    {
      return Err(NovaError::ProofVerifyError);
    }

    // check the satisfiability of the provided instances
    let (res_r_primary, (res_r_secondary, res_l_secondary)) = rayon::join(
      || {
        pp.r1cs_shape_primary
          .is_sat_relaxed(&pp.ck_primary, &self.r_U_primary, &self.r_W_primary)
      },
      || {
        rayon::join(
          || {
            pp.r1cs_shape_secondary.is_sat_relaxed(
              &pp.ck_secondary,
              &self.r_U_secondary,
              &self.r_W_secondary,
            )
          },
          || {
            pp.r1cs_shape_secondary.is_sat(
              &pp.ck_secondary,
              &self.l_u_secondary,
              &self.l_w_secondary,
            )
          },
        )
      },
    );

    // check the returned res objects
    res_r_primary?;
    res_r_secondary?;
    res_l_secondary?;

    Ok((self.zi_primary.clone(), self.zi_secondary.clone()))
  }
}

/// A type that holds the prover key for `CompressedSNARK`
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(bound = "")]
pub struct ProverKey<G1, G2, C1, C2, S1, S2>
where
  G1: Group<Base = <G2 as Group>::Scalar>,
  G2: Group<Base = <G1 as Group>::Scalar>,
  C1: StepCircuit<G1::Scalar>,
  C2: StepCircuit<G2::Scalar>,
  S1: RelaxedR1CSSNARKTrait<G1>,
  S2: RelaxedR1CSSNARKTrait<G2>,
{
  pk_primary: S1::ProverKey,
  pk_secondary: S2::ProverKey,
  _p_c1: PhantomData<C1>,
  _p_c2: PhantomData<C2>,
}

/// A type that holds the verifier key for `CompressedSNARK`
#[derive(Clone, Serialize, Deserialize)]
#[serde(bound = "")]
pub struct VerifierKey<G1, G2, C1, C2, S1, S2>
where
  G1: Group<Base = <G2 as Group>::Scalar>,
  G2: Group<Base = <G1 as Group>::Scalar>,
  C1: StepCircuit<G1::Scalar>,
  C2: StepCircuit<G2::Scalar>,
  S1: RelaxedR1CSSNARKTrait<G1>,
  S2: RelaxedR1CSSNARKTrait<G2>,
{
  F_arity_primary: usize,
  F_arity_secondary: usize,
  ro_consts_primary: ROConstants<G1>,
  ro_consts_secondary: ROConstants<G2>,
  digest: G1::Scalar,
  vk_primary: S1::VerifierKey,
  vk_secondary: S2::VerifierKey,
  _p_c1: PhantomData<C1>,
  _p_c2: PhantomData<C2>,
}

/// A SNARK that proves the knowledge of a valid `RecursiveSNARK`
#[derive(Clone, Serialize, Deserialize)]
#[serde(bound = "")]
pub struct CompressedSNARK<G1, G2, C1, C2, S1, S2>
where
  G1: Group<Base = <G2 as Group>::Scalar>,
  G2: Group<Base = <G1 as Group>::Scalar>,
  C1: StepCircuit<G1::Scalar>,
  C2: StepCircuit<G2::Scalar>,
  S1: RelaxedR1CSSNARKTrait<G1>,
  S2: RelaxedR1CSSNARKTrait<G2>,
{
  r_U_primary: RelaxedR1CSInstance<G1>,
  r_W_snark_primary: S1,

  r_U_secondary: RelaxedR1CSInstance<G2>,
  l_u_secondary: R1CSInstance<G2>,
  nifs_secondary: NIFS<G2>,
  f_W_snark_secondary: S2,

  zn_primary: Vec<G1::Scalar>,
  zn_secondary: Vec<G2::Scalar>,

  _p_c1: PhantomData<C1>,
  _p_c2: PhantomData<C2>,
}

impl<G1, G2, C1, C2, S1, S2> CompressedSNARK<G1, G2, C1, C2, S1, S2>
where
  G1: Group<Base = <G2 as Group>::Scalar>,
  G2: Group<Base = <G1 as Group>::Scalar>,
  C1: StepCircuit<G1::Scalar>,
  C2: StepCircuit<G2::Scalar>,
  S1: RelaxedR1CSSNARKTrait<G1>,
  S2: RelaxedR1CSSNARKTrait<G2>,
{
  /// Creates prover and verifier keys for `CompressedSNARK`
  pub fn setup(
    pp: &PublicParams<G1, G2, C1, C2>,
  ) -> Result<
    (
      ProverKey<G1, G2, C1, C2, S1, S2>,
      VerifierKey<G1, G2, C1, C2, S1, S2>,
    ),
    NovaError,
  > {
    let (pk_primary, vk_primary) = S1::setup(&pp.ck_primary, &pp.r1cs_shape_primary)?;
    let (pk_secondary, vk_secondary) = S2::setup(&pp.ck_secondary, &pp.r1cs_shape_secondary)?;

    let pk = ProverKey {
      pk_primary,
      pk_secondary,
      _p_c1: Default::default(),
      _p_c2: Default::default(),
    };

    let vk = VerifierKey {
      F_arity_primary: pp.F_arity_primary,
      F_arity_secondary: pp.F_arity_secondary,
      ro_consts_primary: pp.ro_consts_primary.clone(),
      ro_consts_secondary: pp.ro_consts_secondary.clone(),
      digest: pp.digest,
      vk_primary,
      vk_secondary,
      _p_c1: Default::default(),
      _p_c2: Default::default(),
    };

    Ok((pk, vk))
  }

  /// Create a new `CompressedSNARK`
  pub fn prove(
    pp: &PublicParams<G1, G2, C1, C2>,
    pk: &ProverKey<G1, G2, C1, C2, S1, S2>,
    recursive_snark: &RecursiveSNARK<G1, G2, C1, C2>,
  ) -> Result<Self, NovaError> {
    // fold the secondary circuit's instance
    let res_secondary = NIFS::prove(
      &pp.ck_secondary,
      &pp.ro_consts_secondary,
      &scalar_as_base::<G1>(pp.digest),
      &pp.r1cs_shape_secondary,
      &recursive_snark.r_U_secondary,
      &recursive_snark.r_W_secondary,
      &recursive_snark.l_u_secondary,
      &recursive_snark.l_w_secondary,
    );

    let (nifs_secondary, (f_U_secondary, f_W_secondary)) = res_secondary?;

    // create SNARKs proving the knowledge of f_W_primary and f_W_secondary
    let (r_W_snark_primary, f_W_snark_secondary) = rayon::join(
      || {
        S1::prove(
          &pp.ck_primary,
          &pk.pk_primary,
          &recursive_snark.r_U_primary,
          &recursive_snark.r_W_primary,
        )
      },
      || {
        S2::prove(
          &pp.ck_secondary,
          &pk.pk_secondary,
          &f_U_secondary,
          &f_W_secondary,
        )
      },
    );

    Ok(Self {
      r_U_primary: recursive_snark.r_U_primary.clone(),
      r_W_snark_primary: r_W_snark_primary?,

      r_U_secondary: recursive_snark.r_U_secondary.clone(),
      l_u_secondary: recursive_snark.l_u_secondary.clone(),
      nifs_secondary,
      f_W_snark_secondary: f_W_snark_secondary?,

      zn_primary: recursive_snark.zi_primary.clone(),
      zn_secondary: recursive_snark.zi_secondary.clone(),

      _p_c1: Default::default(),
      _p_c2: Default::default(),
    })
  }

  /// Verify the correctness of the `CompressedSNARK`
  pub fn verify(
    &self,
    vk: &VerifierKey<G1, G2, C1, C2, S1, S2>,
    num_steps: usize,
    z0_primary: Vec<G1::Scalar>,
    z0_secondary: Vec<G2::Scalar>,
  ) -> Result<(Vec<G1::Scalar>, Vec<G2::Scalar>), NovaError> {
    // number of steps cannot be zero
    if num_steps == 0 {
      return Err(NovaError::ProofVerifyError);
    }

    // check if the (relaxed) R1CS instances have two public outputs
    if self.l_u_secondary.X.len() != 2
      || self.r_U_primary.X.len() != 2
      || self.r_U_secondary.X.len() != 2
    {
      return Err(NovaError::ProofVerifyError);
    }

    // check if the output hashes in R1CS instances point to the right running instances
    let (hash_primary, hash_secondary) = {
      let mut hasher = <G2 as Group>::RO::new(
        vk.ro_consts_secondary.clone(),
        NUM_FE_WITHOUT_IO_FOR_CRHF + 2 * vk.F_arity_primary,
      );
      hasher.absorb(vk.digest);
      hasher.absorb(G1::Scalar::from(num_steps as u64));
      for e in z0_primary {
        hasher.absorb(e);
      }
      for e in &self.zn_primary {
        hasher.absorb(*e);
      }
      self.r_U_secondary.absorb_in_ro(&mut hasher);

      let mut hasher2 = <G1 as Group>::RO::new(
        vk.ro_consts_primary.clone(),
        NUM_FE_WITHOUT_IO_FOR_CRHF + 2 * vk.F_arity_secondary,
      );
      hasher2.absorb(scalar_as_base::<G1>(vk.digest));
      hasher2.absorb(G2::Scalar::from(num_steps as u64));
      for e in z0_secondary {
        hasher2.absorb(e);
      }
      for e in &self.zn_secondary {
        hasher2.absorb(*e);
      }
      self.r_U_primary.absorb_in_ro(&mut hasher2);

      (
        hasher.squeeze(NUM_HASH_BITS),
        hasher2.squeeze(NUM_HASH_BITS),
      )
    };

    if hash_primary != self.l_u_secondary.X[0]
      || hash_secondary != scalar_as_base::<G2>(self.l_u_secondary.X[1])
    {
      return Err(NovaError::ProofVerifyError);
    }

    // fold the running instance and last instance to get a folded instance
    let f_U_secondary = self.nifs_secondary.verify(
      &vk.ro_consts_secondary,
      &scalar_as_base::<G1>(vk.digest),
      &self.r_U_secondary,
      &self.l_u_secondary,
    )?;

    // check the satisfiability of the folded instances using SNARKs proving the knowledge of their satisfying witnesses
    let (res_primary, res_secondary) = rayon::join(
      || {
        self
          .r_W_snark_primary
          .verify(&vk.vk_primary, &self.r_U_primary)
      },
      || {
        self
          .f_W_snark_secondary
          .verify(&vk.vk_secondary, &f_U_secondary)
      },
    );

    res_primary?;
    res_secondary?;

    Ok((self.zn_primary.clone(), self.zn_secondary.clone()))
  }
}

type CommitmentKey<G> = <<G as Group>::CE as CommitmentEngineTrait<G>>::CommitmentKey;
type Commitment<G> = <<G as Group>::CE as CommitmentEngineTrait<G>>::Commitment;
type CompressedCommitment<G> = <<<G as Group>::CE as CommitmentEngineTrait<G>>::Commitment as CommitmentTrait<G>>::CompressedCommitment;
type CE<G> = <G as Group>::CE;

fn compute_digest<G: Group, T: Serialize>(o: &T) -> G::Scalar {
  // obtain a vector of bytes representing public parameters
  let bytes = bincode::serialize(o).unwrap();
  // convert pp_bytes into a short digest
  let mut hasher = Sha3_256::new();
  hasher.update(&bytes);
  let digest = hasher.finalize();

  // truncate the digest to NUM_HASH_BITS bits
  let bv = (0..NUM_HASH_BITS).map(|i| {
    let (byte_pos, bit_pos) = (i / 8, i % 8);
    let bit = (digest[byte_pos] >> bit_pos) & 1;
    bit == 1
  });

  // turn the bit vector into a scalar
  let mut digest = G::Scalar::ZERO;
  let mut coeff = G::Scalar::ONE;
  for bit in bv {
    if bit {
      digest += coeff;
    }
    coeff += coeff;
  }
  digest
}

#[cfg(test)]
mod tests {
  use crate::provider::bn256_grumpkin::{bn256, grumpkin};
  use crate::provider::pedersen::CommitmentKeyExtTrait;
  use crate::provider::secp_secq::{secp256k1, secq256k1};

  use super::*;
  type EE1<G1> = provider::ipa_pc::EvaluationEngine<G1>;
  type EE2<G2> = provider::ipa_pc::EvaluationEngine<G2>;
  type S1<G1> = spartan::snark::RelaxedR1CSSNARK<G1, EE1<G1>>;
  type S2<G2> = spartan::snark::RelaxedR1CSSNARK<G2, EE2<G2>>;
  type S1Prime<G1> = spartan::ppsnark::RelaxedR1CSSNARK<G1, EE1<G1>>;
  type S2Prime<G2> = spartan::ppsnark::RelaxedR1CSSNARK<G2, EE2<G2>>;

  use ::bellpepper_core::{num::AllocatedNum, ConstraintSystem, SynthesisError};
  use core::marker::PhantomData;
  use ff::PrimeField;
  use traits::circuit::TrivialTestCircuit;

  #[derive(Clone, Debug, Default)]
  struct CubicCircuit<F: PrimeField> {
    _p: PhantomData<F>,
  }

  impl<F> StepCircuit<F> for CubicCircuit<F>
  where
    F: PrimeField,
  {
    fn arity(&self) -> usize {
      1
    }

    fn synthesize<CS: ConstraintSystem<F>>(
      &self,
      cs: &mut CS,
      z: &[AllocatedNum<F>],
    ) -> Result<Vec<AllocatedNum<F>>, SynthesisError> {
      // Consider a cubic equation: `x^3 + x + 5 = y`, where `x` and `y` are respectively the input and output.
      let x = &z[0];
      let x_sq = x.square(cs.namespace(|| "x_sq"))?;
      let x_cu = x_sq.mul(cs.namespace(|| "x_cu"), x)?;
      let y = AllocatedNum::alloc(cs.namespace(|| "y"), || {
        Ok(x_cu.get_value().unwrap() + x.get_value().unwrap() + F::from(5u64))
      })?;

      cs.enforce(
        || "y = x^3 + x + 5",
        |lc| {
          lc + x_cu.get_variable()
            + x.get_variable()
            + CS::one()
            + CS::one()
            + CS::one()
            + CS::one()
            + CS::one()
        },
        |lc| lc + CS::one(),
        |lc| lc + y.get_variable(),
      );

      Ok(vec![y])
    }
  }

  impl<F> CubicCircuit<F>
  where
    F: PrimeField,
  {
    fn output(&self, z: &[F]) -> Vec<F> {
      vec![z[0] * z[0] * z[0] + z[0] + F::from(5u64)]
    }
  }

  fn test_pp_digest_with<G1, G2, T1, T2>(circuit1: &T1, circuit2: &T2, expected: &str)
  where
    G1: Group<Base = <G2 as Group>::Scalar>,
    G2: Group<Base = <G1 as Group>::Scalar>,
    T1: StepCircuit<G1::Scalar>,
    T2: StepCircuit<G2::Scalar>,
  {
    let pp = PublicParams::<G1, G2, T1, T2>::setup(circuit1, circuit2);

    let digest_str = pp
      .digest
      .to_repr()
      .as_ref()
      .iter()
      .map(|b| format!("{b:02x}"))
      .collect::<String>();
    assert_eq!(digest_str, expected);
  }

  #[test]
  fn test_pp_digest() {
    type G1 = pasta_curves::pallas::Point;
    type G2 = pasta_curves::vesta::Point;
    let trivial_circuit1 = TrivialTestCircuit::<<G1 as Group>::Scalar>::default();
    let trivial_circuit2 = TrivialTestCircuit::<<G2 as Group>::Scalar>::default();
    let cubic_circuit1 = CubicCircuit::<<G1 as Group>::Scalar>::default();

    test_pp_digest_with::<G1, G2, _, _>(
      &trivial_circuit1,
      &trivial_circuit2,
      "39a4ea9dd384346fdeb6b5857c7be56fa035153b616d55311f3191dfbceea603",
    );

    test_pp_digest_with::<G1, G2, _, _>(
      &cubic_circuit1,
      &trivial_circuit2,
      "3f7b25f589f2da5ab26254beba98faa54f6442ebf5fa5860caf7b08b576cab00",
    );

    let trivial_circuit1_grumpkin =
      TrivialTestCircuit::<<bn256::Point as Group>::Scalar>::default();
    let trivial_circuit2_grumpkin =
      TrivialTestCircuit::<<grumpkin::Point as Group>::Scalar>::default();
    let cubic_circuit1_grumpkin = CubicCircuit::<<bn256::Point as Group>::Scalar>::default();

    test_pp_digest_with::<bn256::Point, grumpkin::Point, _, _>(
      &trivial_circuit1_grumpkin,
      &trivial_circuit2_grumpkin,
      "967acca1d6b4731cd65d4072c12bbaca9648f24d7bcc2877aee720e4265d4302",
    );
    test_pp_digest_with::<bn256::Point, grumpkin::Point, _, _>(
      &cubic_circuit1_grumpkin,
      &trivial_circuit2_grumpkin,
      "44629f26a78bf6c4e3077f940232050d1793d304fdba5e221d0cf66f76a37903",
    );

    let trivial_circuit1_secp =
      TrivialTestCircuit::<<secp256k1::Point as Group>::Scalar>::default();
    let trivial_circuit2_secp =
      TrivialTestCircuit::<<secq256k1::Point as Group>::Scalar>::default();
    let cubic_circuit1_secp = CubicCircuit::<<secp256k1::Point as Group>::Scalar>::default();

    test_pp_digest_with::<secp256k1::Point, secq256k1::Point, _, _>(
      &trivial_circuit1_secp,
      &trivial_circuit2_secp.clone(),
      "b99760668a42354643e17b2f0a2d54f173d237eb213e7e758b20a88b4c653c01",
    );
    test_pp_digest_with::<secp256k1::Point, secq256k1::Point, _, _>(
      &cubic_circuit1_secp,
      &trivial_circuit2_secp,
      "68db620e610a3cd75146a1e1bdd168f486b82c0b670277ad1e3d50441c501502",
    );
  }

  fn test_ivc_trivial_with<G1, G2>()
  where
    G1: Group<Base = <G2 as Group>::Scalar>,
    G2: Group<Base = <G1 as Group>::Scalar>,
  {
    let test_circuit1 = TrivialTestCircuit::<<G1 as Group>::Scalar>::default();
    let test_circuit2 = TrivialTestCircuit::<<G2 as Group>::Scalar>::default();

    // produce public parameters
    let pp = PublicParams::<
      G1,
      G2,
      TrivialTestCircuit<<G1 as Group>::Scalar>,
      TrivialTestCircuit<<G2 as Group>::Scalar>,
    >::setup(&test_circuit1, &test_circuit2);

    let num_steps = 1;

    // produce a recursive SNARK
    let mut recursive_snark = RecursiveSNARK::new(
      &pp,
      &test_circuit1,
      &test_circuit2,
      vec![<G1 as Group>::Scalar::ZERO],
      vec![<G2 as Group>::Scalar::ZERO],
    );

    let res = recursive_snark.prove_step(
      &pp,
      &test_circuit1,
      &test_circuit2,
      vec![<G1 as Group>::Scalar::ZERO],
      vec![<G2 as Group>::Scalar::ZERO],
    );

    assert!(res.is_ok());

    // verify the recursive SNARK
    let res = recursive_snark.verify(
      &pp,
      num_steps,
      &[<G1 as Group>::Scalar::ZERO],
      &[<G2 as Group>::Scalar::ZERO],
    );
    assert!(res.is_ok());
  }

  #[test]
  fn test_ivc_trivial() {
    type G1 = pasta_curves::pallas::Point;
    type G2 = pasta_curves::vesta::Point;

    test_ivc_trivial_with::<G1, G2>();
    test_ivc_trivial_with::<bn256::Point, grumpkin::Point>();
    test_ivc_trivial_with::<secp256k1::Point, secq256k1::Point>();
  }

  fn test_ivc_nontrivial_with<G1, G2>()
  where
    G1: Group<Base = <G2 as Group>::Scalar>,
    G2: Group<Base = <G1 as Group>::Scalar>,
  {
    let circuit_primary = TrivialTestCircuit::default();
    let circuit_secondary = CubicCircuit::default();

    // produce public parameters
    let pp = PublicParams::<
      G1,
      G2,
      TrivialTestCircuit<<G1 as Group>::Scalar>,
      CubicCircuit<<G2 as Group>::Scalar>,
    >::setup(&circuit_primary, &circuit_secondary);

    let num_steps = 3;

    // produce a recursive SNARK
    let mut recursive_snark = RecursiveSNARK::<
      G1,
      G2,
      TrivialTestCircuit<<G1 as Group>::Scalar>,
      CubicCircuit<<G2 as Group>::Scalar>,
    >::new(
      &pp,
      &circuit_primary,
      &circuit_secondary,
      vec![<G1 as Group>::Scalar::ONE],
      vec![<G2 as Group>::Scalar::ZERO],
    );

    for i in 0..num_steps {
      let res = recursive_snark.prove_step(
        &pp,
        &circuit_primary,
        &circuit_secondary,
        vec![<G1 as Group>::Scalar::ONE],
        vec![<G2 as Group>::Scalar::ZERO],
      );
      assert!(res.is_ok());

      // verify the recursive snark at each step of recursion
      let res = recursive_snark.verify(
        &pp,
        i + 1,
        &[<G1 as Group>::Scalar::ONE],
        &[<G2 as Group>::Scalar::ZERO],
      );
      assert!(res.is_ok());
    }

    // verify the recursive SNARK
    let res = recursive_snark.verify(
      &pp,
      num_steps,
      &[<G1 as Group>::Scalar::ONE],
      &[<G2 as Group>::Scalar::ZERO],
    );
    assert!(res.is_ok());

    let (zn_primary, zn_secondary) = res.unwrap();

    // sanity: check the claimed output with a direct computation of the same
    assert_eq!(zn_primary, vec![<G1 as Group>::Scalar::ONE]);
    let mut zn_secondary_direct = vec![<G2 as Group>::Scalar::ZERO];
    for _i in 0..num_steps {
      zn_secondary_direct = circuit_secondary.clone().output(&zn_secondary_direct);
    }
    assert_eq!(zn_secondary, zn_secondary_direct);
    assert_eq!(zn_secondary, vec![<G2 as Group>::Scalar::from(2460515u64)]);
  }

  #[test]
  fn test_ivc_nontrivial() {
    type G1 = pasta_curves::pallas::Point;
    type G2 = pasta_curves::vesta::Point;

    test_ivc_nontrivial_with::<G1, G2>();
    test_ivc_nontrivial_with::<bn256::Point, grumpkin::Point>();
    test_ivc_nontrivial_with::<secp256k1::Point, secq256k1::Point>();
  }

  fn test_ivc_nontrivial_with_compression_with<G1, G2>()
  where
    G1: Group<Base = <G2 as Group>::Scalar>,
    G2: Group<Base = <G1 as Group>::Scalar>,
    // this is due to the reliance on CommitmentKeyExtTrait as a bound in ipa_pc
    <G1::CE as CommitmentEngineTrait<G1>>::CommitmentKey: CommitmentKeyExtTrait<G1>,
    <G2::CE as CommitmentEngineTrait<G2>>::CommitmentKey: CommitmentKeyExtTrait<G2>,
  {
    let circuit_primary = TrivialTestCircuit::default();
    let circuit_secondary = CubicCircuit::default();

    // produce public parameters
    let pp = PublicParams::<
      G1,
      G2,
      TrivialTestCircuit<<G1 as Group>::Scalar>,
      CubicCircuit<<G2 as Group>::Scalar>,
    >::setup(&circuit_primary, &circuit_secondary);

    let num_steps = 3;

    // produce a recursive SNARK
    let mut recursive_snark = RecursiveSNARK::<
      G1,
      G2,
      TrivialTestCircuit<<G1 as Group>::Scalar>,
      CubicCircuit<<G2 as Group>::Scalar>,
    >::new(
      &pp,
      &circuit_primary,
      &circuit_secondary,
      vec![<G1 as Group>::Scalar::ONE],
      vec![<G2 as Group>::Scalar::ZERO],
    );

    for _i in 0..num_steps {
      let res = recursive_snark.prove_step(
        &pp,
        &circuit_primary,
        &circuit_secondary,
        vec![<G1 as Group>::Scalar::ONE],
        vec![<G2 as Group>::Scalar::ZERO],
      );
      assert!(res.is_ok());
    }

    // verify the recursive SNARK
    let res = recursive_snark.verify(
      &pp,
      num_steps,
      &[<G1 as Group>::Scalar::ONE],
      &[<G2 as Group>::Scalar::ZERO],
    );
    assert!(res.is_ok());

    let (zn_primary, zn_secondary) = res.unwrap();

    // sanity: check the claimed output with a direct computation of the same
    assert_eq!(zn_primary, vec![<G1 as Group>::Scalar::ONE]);
    let mut zn_secondary_direct = vec![<G2 as Group>::Scalar::ZERO];
    for _i in 0..num_steps {
      zn_secondary_direct = circuit_secondary.clone().output(&zn_secondary_direct);
    }
    assert_eq!(zn_secondary, zn_secondary_direct);
    assert_eq!(zn_secondary, vec![<G2 as Group>::Scalar::from(2460515u64)]);

    // produce the prover and verifier keys for compressed snark
    let (pk, vk) = CompressedSNARK::<_, _, _, _, S1<G1>, S2<G2>>::setup(&pp).unwrap();

    // produce a compressed SNARK
    let res = CompressedSNARK::<_, _, _, _, S1<G1>, S2<G2>>::prove(&pp, &pk, &recursive_snark);
    assert!(res.is_ok());
    let compressed_snark = res.unwrap();

    // verify the compressed SNARK
    let res = compressed_snark.verify(
      &vk,
      num_steps,
      vec![<G1 as Group>::Scalar::ONE],
      vec![<G2 as Group>::Scalar::ZERO],
    );
    assert!(res.is_ok());
  }

  #[test]
  fn test_ivc_nontrivial_with_compression() {
    type G1 = pasta_curves::pallas::Point;
    type G2 = pasta_curves::vesta::Point;

    test_ivc_nontrivial_with_compression_with::<G1, G2>();
    test_ivc_nontrivial_with_compression_with::<bn256::Point, grumpkin::Point>();
    test_ivc_nontrivial_with_compression_with::<secp256k1::Point, secq256k1::Point>();
  }

  fn test_ivc_nontrivial_with_spark_compression_with<G1, G2>()
  where
    G1: Group<Base = <G2 as Group>::Scalar>,
    G2: Group<Base = <G1 as Group>::Scalar>,
    // this is due to the reliance on CommitmentKeyExtTrait as a bound in ipa_pc
    <G1::CE as CommitmentEngineTrait<G1>>::CommitmentKey: CommitmentKeyExtTrait<G1>,
    <G2::CE as CommitmentEngineTrait<G2>>::CommitmentKey: CommitmentKeyExtTrait<G2>,
  {
    let circuit_primary = TrivialTestCircuit::default();
    let circuit_secondary = CubicCircuit::default();

    // produce public parameters
    let pp = PublicParams::<
      G1,
      G2,
      TrivialTestCircuit<<G1 as Group>::Scalar>,
      CubicCircuit<<G2 as Group>::Scalar>,
    >::setup(&circuit_primary, &circuit_secondary);

    let num_steps = 3;

    // produce a recursive SNARK
    let mut recursive_snark = RecursiveSNARK::<
      G1,
      G2,
      TrivialTestCircuit<<G1 as Group>::Scalar>,
      CubicCircuit<<G2 as Group>::Scalar>,
    >::new(
      &pp,
      &circuit_primary,
      &circuit_secondary,
      vec![<G1 as Group>::Scalar::ONE],
      vec![<G2 as Group>::Scalar::ZERO],
    );

    for _i in 0..num_steps {
      let res = recursive_snark.prove_step(
        &pp,
        &circuit_primary,
        &circuit_secondary,
        vec![<G1 as Group>::Scalar::ONE],
        vec![<G2 as Group>::Scalar::ZERO],
      );
      assert!(res.is_ok());
    }

    // verify the recursive SNARK
    let res = recursive_snark.verify(
      &pp,
      num_steps,
      &[<G1 as Group>::Scalar::ONE],
      &[<G2 as Group>::Scalar::ZERO],
    );
    assert!(res.is_ok());

    let (zn_primary, zn_secondary) = res.unwrap();

    // sanity: check the claimed output with a direct computation of the same
    assert_eq!(zn_primary, vec![<G1 as Group>::Scalar::ONE]);
    let mut zn_secondary_direct = vec![<G2 as Group>::Scalar::ZERO];
    for _i in 0..num_steps {
      zn_secondary_direct = CubicCircuit::default().output(&zn_secondary_direct);
    }
    assert_eq!(zn_secondary, zn_secondary_direct);
    assert_eq!(zn_secondary, vec![<G2 as Group>::Scalar::from(2460515u64)]);

    // run the compressed snark with Spark compiler

    // produce the prover and verifier keys for compressed snark
    let (pk, vk) = CompressedSNARK::<_, _, _, _, S1Prime<G1>, S2Prime<G2>>::setup(&pp).unwrap();

    // produce a compressed SNARK
    let res =
      CompressedSNARK::<_, _, _, _, S1Prime<G1>, S2Prime<G2>>::prove(&pp, &pk, &recursive_snark);
    assert!(res.is_ok());
    let compressed_snark = res.unwrap();

    // verify the compressed SNARK
    let res = compressed_snark.verify(
      &vk,
      num_steps,
      vec![<G1 as Group>::Scalar::ONE],
      vec![<G2 as Group>::Scalar::ZERO],
    );
    assert!(res.is_ok());
  }

  #[test]
  fn test_ivc_nontrivial_with_spark_compression() {
    type G1 = pasta_curves::pallas::Point;
    type G2 = pasta_curves::vesta::Point;

    test_ivc_nontrivial_with_spark_compression_with::<G1, G2>();
    test_ivc_nontrivial_with_spark_compression_with::<bn256::Point, grumpkin::Point>();
    test_ivc_nontrivial_with_spark_compression_with::<secp256k1::Point, secq256k1::Point>();
  }

  fn test_ivc_nondet_with_compression_with<G1, G2>()
  where
    G1: Group<Base = <G2 as Group>::Scalar>,
    G2: Group<Base = <G1 as Group>::Scalar>,
    // this is due to the reliance on CommitmentKeyExtTrait as a bound in ipa_pc
    <G1::CE as CommitmentEngineTrait<G1>>::CommitmentKey: CommitmentKeyExtTrait<G1>,
    <G2::CE as CommitmentEngineTrait<G2>>::CommitmentKey: CommitmentKeyExtTrait<G2>,
  {
    // y is a non-deterministic advice representing the fifth root of the input at a step.
    #[derive(Clone, Debug)]
    struct FifthRootCheckingCircuit<F: PrimeField> {
      y: F,
    }

    impl<F> FifthRootCheckingCircuit<F>
    where
      F: PrimeField,
    {
      fn new(num_steps: usize) -> (Vec<F>, Vec<Self>) {
        let mut powers = Vec::new();
        let rng = &mut rand::rngs::OsRng;
        let mut seed = F::random(rng);
        for _i in 0..num_steps + 1 {
          seed *= seed.clone().square().square();

          powers.push(Self { y: seed });
        }

        // reverse the powers to get roots
        let roots = powers.into_iter().rev().collect::<Vec<Self>>();
        (vec![roots[0].y], roots[1..].to_vec())
      }
    }

    impl<F> StepCircuit<F> for FifthRootCheckingCircuit<F>
    where
      F: PrimeField,
    {
      fn arity(&self) -> usize {
        1
      }

      fn synthesize<CS: ConstraintSystem<F>>(
        &self,
        cs: &mut CS,
        z: &[AllocatedNum<F>],
      ) -> Result<Vec<AllocatedNum<F>>, SynthesisError> {
        let x = &z[0];

        // we allocate a variable and set it to the provided non-deterministic advice.
        let y = AllocatedNum::alloc(cs.namespace(|| "y"), || Ok(self.y))?;

        // We now check if y = x^{1/5} by checking if y^5 = x
        let y_sq = y.square(cs.namespace(|| "y_sq"))?;
        let y_quad = y_sq.square(cs.namespace(|| "y_quad"))?;
        let y_pow_5 = y_quad.mul(cs.namespace(|| "y_fifth"), &y)?;

        cs.enforce(
          || "y^5 = x",
          |lc| lc + y_pow_5.get_variable(),
          |lc| lc + CS::one(),
          |lc| lc + x.get_variable(),
        );

        Ok(vec![y])
      }
    }

    let circuit_primary = FifthRootCheckingCircuit {
      y: <G1 as Group>::Scalar::ZERO,
    };

    let circuit_secondary = TrivialTestCircuit::default();

    // produce public parameters
    let pp = PublicParams::<
      G1,
      G2,
      FifthRootCheckingCircuit<<G1 as Group>::Scalar>,
      TrivialTestCircuit<<G2 as Group>::Scalar>,
    >::setup(&circuit_primary, &circuit_secondary);

    let num_steps = 3;

    // produce non-deterministic advice
    let (z0_primary, roots) = FifthRootCheckingCircuit::new(num_steps);
    let z0_secondary = vec![<G2 as Group>::Scalar::ZERO];

    // produce a recursive SNARK
    let mut recursive_snark: RecursiveSNARK<
      G1,
      G2,
      FifthRootCheckingCircuit<<G1 as Group>::Scalar>,
      TrivialTestCircuit<<G2 as Group>::Scalar>,
    > = RecursiveSNARK::<
      G1,
      G2,
      FifthRootCheckingCircuit<<G1 as Group>::Scalar>,
      TrivialTestCircuit<<G2 as Group>::Scalar>,
    >::new(
      &pp,
      &roots[0],
      &circuit_secondary,
      z0_primary.clone(),
      z0_secondary.clone(),
    );

    for circuit_primary in roots.iter().take(num_steps) {
      let res = recursive_snark.prove_step(
        &pp,
        circuit_primary,
        &circuit_secondary.clone(),
        z0_primary.clone(),
        z0_secondary.clone(),
      );
      assert!(res.is_ok());
    }

    // verify the recursive SNARK
    let res = recursive_snark.verify(&pp, num_steps, &z0_primary, &z0_secondary);
    assert!(res.is_ok());

    // produce the prover and verifier keys for compressed snark
    let (pk, vk) = CompressedSNARK::<_, _, _, _, S1<G1>, S2<G2>>::setup(&pp).unwrap();

    // produce a compressed SNARK
    let res = CompressedSNARK::<_, _, _, _, S1<G1>, S2<G2>>::prove(&pp, &pk, &recursive_snark);
    assert!(res.is_ok());
    let compressed_snark = res.unwrap();

    // verify the compressed SNARK
    let res = compressed_snark.verify(&vk, num_steps, z0_primary, z0_secondary);
    assert!(res.is_ok());
  }

  #[test]
  fn test_ivc_nondet_with_compression() {
    type G1 = pasta_curves::pallas::Point;
    type G2 = pasta_curves::vesta::Point;

    test_ivc_nondet_with_compression_with::<G1, G2>();
    test_ivc_nondet_with_compression_with::<bn256::Point, grumpkin::Point>();
    test_ivc_nondet_with_compression_with::<secp256k1::Point, secq256k1::Point>();
  }

  fn test_ivc_base_with<G1, G2>()
  where
    G1: Group<Base = <G2 as Group>::Scalar>,
    G2: Group<Base = <G1 as Group>::Scalar>,
  {
    let test_circuit1 = TrivialTestCircuit::<<G1 as Group>::Scalar>::default();
    let test_circuit2 = CubicCircuit::<<G2 as Group>::Scalar>::default();

    // produce public parameters
    let pp = PublicParams::<
      G1,
      G2,
      TrivialTestCircuit<<G1 as Group>::Scalar>,
      CubicCircuit<<G2 as Group>::Scalar>,
    >::setup(&test_circuit1, &test_circuit2);

    let num_steps = 1;

    // produce a recursive SNARK
    let mut recursive_snark = RecursiveSNARK::<
      G1,
      G2,
      TrivialTestCircuit<<G1 as Group>::Scalar>,
      CubicCircuit<<G2 as Group>::Scalar>,
    >::new(
      &pp,
      &test_circuit1,
      &test_circuit2,
      vec![<G1 as Group>::Scalar::ONE],
      vec![<G2 as Group>::Scalar::ZERO],
    );

    // produce a recursive SNARK
    let res = recursive_snark.prove_step(
      &pp,
      &test_circuit1,
      &test_circuit2,
      vec![<G1 as Group>::Scalar::ONE],
      vec![<G2 as Group>::Scalar::ZERO],
    );

    assert!(res.is_ok());

    // verify the recursive SNARK
    let res = recursive_snark.verify(
      &pp,
      num_steps,
      &[<G1 as Group>::Scalar::ONE],
      &[<G2 as Group>::Scalar::ZERO],
    );
    assert!(res.is_ok());

    let (zn_primary, zn_secondary) = res.unwrap();

    assert_eq!(zn_primary, vec![<G1 as Group>::Scalar::ONE]);
    assert_eq!(zn_secondary, vec![<G2 as Group>::Scalar::from(5u64)]);
  }

  #[test]
  fn test_ivc_base() {
    type G1 = pasta_curves::pallas::Point;
    type G2 = pasta_curves::vesta::Point;

    test_ivc_base_with::<G1, G2>();
    test_ivc_base_with::<bn256::Point, grumpkin::Point>();
    test_ivc_base_with::<secp256k1::Point, secq256k1::Point>();
  }
}
