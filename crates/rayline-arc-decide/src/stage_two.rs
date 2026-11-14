//! Stage two's network: scores every trained arm (a model at one thinking
//! control) for one request.
//!
//! The network reads the decision readout, the mean of the blocks'
//! readouts with each block's role and the arm that produced it, and the
//! arm's own vector. Its quality and usage heads do not depend on prices;
//! an arm's cost is its predicted usage at the request's rates. Arithmetic
//! is float32, as in the reference.

use crate::stage_one::LEGS;
use crate::tensors::{TensorError, Tensors};

/// Roles in the order of the network's role table.
const ROLES: [&str; 4] = ["system", "user", "assistant", "tool"];
/// The usage head's unit, tokens.
const USAGE_SCALE: f32 = 1_000_000.0;

#[derive(Debug)]
struct Linear {
    rows: usize,
    columns: usize,
    weight: Vec<f32>,
    bias: Vec<f32>,
}

impl Linear {
    fn load(tensors: &Tensors, name: &str) -> Result<Self, TensorError> {
        let &[rows, columns] = tensors.shape(&format!("{name}.weight"))? else {
            return Err(TensorError(format!("{name}.weight is not a matrix")));
        };
        Ok(Self {
            rows,
            columns,
            weight: tensors.f32(&format!("{name}.weight"), &[rows, columns])?,
            bias: tensors.f32(&format!("{name}.bias"), &[rows])?,
        })
    }

    /// `bias + weight[:, offset..offset + input.len()] · input`, added to `out`
    /// when `accumulate`, else replacing it.
    fn apply(&self, input: &[f32], offset: usize, out: &mut [f32], accumulate: bool) {
        for (row, value) in out.iter_mut().enumerate() {
            let weights = &self.weight[row * self.columns + offset..][..input.len()];
            let sum: f32 = weights.iter().zip(input).map(|(w, x)| w * x).sum();
            *value = if accumulate {
                *value + sum
            } else {
                self.bias[row] + sum
            };
        }
    }
}

/// One arm's head outputs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ArmScore {
    pub quality: f32,
    pub usage: [f32; LEGS],
}

#[derive(Debug)]
pub struct StageTwo {
    arms: usize,
    dim: usize,
    hidden: usize,
    arm_dim: usize,
    role_dim: usize,
    /// Each arm's vector, `[arm][arm_dim]`; row 0 of the tables (no arm) is
    /// not stored here.
    arm_vectors: Vec<f32>,
    roles: Vec<f32>,
    context: Linear,
    history: Linear,
    trunk_in: Linear,
    trunk_out: Linear,
    quality: Linear,
    usage: Linear,
    /// Whether the fit supports an arm's quality head, and its usage head.
    pub quality_support: Vec<bool>,
    pub usage_support: Vec<bool>,
}

fn softplus(x: f32) -> f32 {
    if x > 20.0 { x } else { x.exp().ln_1p() }
}

fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

impl StageTwo {
    /// Reads the network from the release's `stage_two.*` tensors.
    /// `support_columns` names the columns of `meta.support`.
    pub fn load(tensors: &Tensors, support_columns: &[String]) -> Result<Self, TensorError> {
        let &[rows] = tensors.shape("arms.model_index")? else {
            return Err(TensorError("arms.model_index is not a vector".into()));
        };
        let arms = rows
            .checked_sub(1)
            .ok_or_else(|| TensorError("the arm table is empty".into()))?;
        let mut parts = Vec::new();
        let mut arm_dim = 0;
        for part in ["model", "effort", "configuration", "thinking"] {
            let name = format!("arms.{part}_vectors.weight");
            let &[count, width] = tensors.shape(&name)? else {
                return Err(TensorError(format!("{name} is not a matrix")));
            };
            let index = tensors.i64(&format!("arms.{part}_index"), &[rows])?;
            if index.iter().any(|i| *i < 0 || *i as usize >= count) {
                return Err(TensorError(format!(
                    "arms.{part}_index names a missing row"
                )));
            }
            parts.push((index, tensors.f32(&name, &[count, width])?, width));
            arm_dim += width;
        }
        let mut arm_vectors = Vec::with_capacity(arms * arm_dim);
        for arm in 1..rows {
            for (index, table, width) in &parts {
                let row = index[arm] as usize;
                arm_vectors.extend_from_slice(&table[row * width..(row + 1) * width]);
            }
        }
        let &[role_count, role_dim] = tensors.shape("roles.weight")? else {
            return Err(TensorError("roles.weight is not a matrix".into()));
        };
        let context = Linear::load(tensors, "context")?;
        let history = Linear::load(tensors, "history_projection")?;
        let trunk_in = Linear::load(tensors, "trunk.0")?;
        let trunk_out = Linear::load(tensors, "trunk.3")?;
        let quality = Linear::load(tensors, "quality")?;
        let usage = Linear::load(tensors, "log_usage")?;
        let (dim, hidden) = (context.columns, context.rows);
        if role_count != ROLES.len()
            || history.columns != dim + arm_dim + role_dim
            || history.rows != hidden
            || trunk_in.columns != hidden * 3 + arm_dim
            || trunk_in.rows != hidden
            || trunk_out.columns != hidden
            || trunk_out.rows != hidden
            || quality.columns != hidden
            || quality.rows != 1
            || usage.columns != hidden
            || usage.rows != LEGS
        {
            return Err(TensorError(
                "the stage-two tensors are not the network this crate computes".into(),
            ));
        }
        let width = support_columns.len();
        let support = tensors.i64("meta.support", &[arms, width])?;
        let column = |name: &str| {
            support_columns
                .iter()
                .position(|c| c == name)
                .ok_or_else(|| TensorError(format!("the release names no support column {name}")))
        };
        let quality_column = column("quality")?;
        let usage_columns = (0..LEGS)
            .map(|leg| column(&format!("usage.{leg}")))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            arms,
            dim,
            hidden,
            arm_dim,
            role_dim,
            arm_vectors,
            roles: tensors.f32("roles.weight", &[role_count, role_dim])?,
            context,
            history,
            trunk_in,
            trunk_out,
            quality,
            usage,
            quality_support: (0..arms)
                .map(|arm| support[arm * width + quality_column] > 0)
                .collect(),
            usage_support: (0..arms)
                .map(|arm| usage_columns.iter().all(|c| support[arm * width + c] > 0))
                .collect(),
        })
    }

    pub fn arms(&self) -> usize {
        self.arms
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Every arm's quality and usage for one request. `readouts` holds one
    /// row per block, `roles` each block's role and `sources` the arm that
    /// produced it, where known.
    pub fn predict(
        &self,
        readouts: &[Vec<f32>],
        roles: &[&str],
        sources: &[Option<usize>],
    ) -> Result<Vec<ArmScore>, TensorError> {
        let steps = readouts.len();
        if steps == 0 || roles.len() != steps || sources.len() != steps {
            return Err(TensorError("readouts, roles and sources must align".into()));
        }
        let mut pooled = vec![0f32; self.dim + self.arm_dim + self.role_dim];
        for ((readout, role), source) in readouts.iter().zip(roles).zip(sources) {
            if readout.len() != self.dim {
                return Err(TensorError("a readout is not the encoder's width".into()));
            }
            let role = ROLES
                .iter()
                .position(|r| r == role)
                .ok_or_else(|| TensorError("a block has an unknown role".into()))?;
            for (sum, value) in pooled.iter_mut().zip(readout) {
                *sum += value;
            }
            if let Some(arm) = source {
                let vector = self
                    .arm_vectors
                    .get(arm * self.arm_dim..(arm + 1) * self.arm_dim)
                    .ok_or_else(|| TensorError("a block names an arm outside the table".into()))?;
                for (sum, value) in pooled[self.dim..].iter_mut().zip(vector) {
                    *sum += value;
                }
            }
            let embedding = &self.roles[role * self.role_dim..(role + 1) * self.role_dim];
            for (sum, value) in pooled[self.dim + self.arm_dim..].iter_mut().zip(embedding) {
                *sum += value;
            }
        }
        for sum in &mut pooled {
            *sum /= steps as f32;
        }
        let hidden = self.hidden;
        let mut shared = vec![0f32; 2 * hidden];
        let (semantic, state) = shared.split_at_mut(hidden);
        self.context.apply(&readouts[steps - 1], 0, semantic, false);
        self.history.apply(&pooled, 0, state, false);
        // The trunk's first layer reads [semantic, state, evidence, arm]; the
        // evidence slot is zero (this recipe retrieves nothing), and the first
        // two are the same for every arm.
        let mut base = vec![0f32; hidden];
        self.trunk_in.apply(&shared, 0, &mut base, false);
        let arm_offset = 3 * hidden;

        let mut scores = Vec::with_capacity(self.arms);
        let (mut first, mut latent) = (vec![0f32; hidden], vec![0f32; hidden]);
        for arm in 0..self.arms {
            first.copy_from_slice(&base);
            let vector = &self.arm_vectors[arm * self.arm_dim..(arm + 1) * self.arm_dim];
            self.trunk_in.apply(vector, arm_offset, &mut first, true);
            for value in &mut first {
                *value = value.max(0.0);
            }
            self.trunk_out.apply(&first, 0, &mut latent, false);
            for value in &mut latent {
                *value = value.max(0.0);
            }
            let mut quality = [0f32; 1];
            self.quality.apply(&latent, 0, &mut quality, false);
            let mut usage = [0f32; LEGS];
            self.usage.apply(&latent, 0, &mut usage, false);
            let score = ArmScore {
                quality: sigmoid(quality[0]),
                usage: usage.map(|raw| softplus(raw) * USAGE_SCALE),
            };
            if !score.quality.is_finite() || score.usage.iter().any(|u| !u.is_finite()) {
                return Err(TensorError("stage two predicted a non-finite value".into()));
            }
            scores.push(score);
        }
        Ok(scores)
    }
}

/// An arm's cost in dollars at `rates` (per million tokens; an unknown leg
/// counts as zero here and is excluded by the caller's price mask).
pub fn cost(usage: &[f32; LEGS], rates: &[Option<f64>; LEGS]) -> f32 {
    let total: f32 = usage
        .iter()
        .zip(rates)
        .map(|(tokens, rate)| tokens * rate.unwrap_or(0.0) as f32)
        .sum();
    total / 1_000_000.0
}
