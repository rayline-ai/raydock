//! Stage one's head: from the decision readout to, per candidate model,
//! the probability the task passes and the tokens the rest of the episode
//! will use on each billed leg.
//!
//! This is the arithmetic of the release's published head module (the
//! `stage_one.*` tensors and the torch code shipped with the vLLM plugin),
//! in float64 as there: an ensemble of members, each a logistic head with a
//! small network over the readout and a log-linear usage head, a usage
//! orderer shared by the members, and a calibrated mean over the members.
//! `NaN` is "unknown" throughout and is never read as zero.

use crate::tensors::{TensorError, Tensors};

/// The billed legs, in the order every usage and rate vector uses.
pub const LEGS: usize = 4;
/// The usage orderer's thinnest usable pool and its token floor.
const MIN_POOL: i64 = 20;
const FLOOR_TOKENS: f64 = 1.0;
/// A calibration map's clip of p away from 0 and 1.
const PLATT_EPS: f64 = 1e-6;

/// Tensors of head variants this crate does not compute. A release that
/// carries one is refused rather than scored without it.
const UNSUPPORTED: &[&str] = &[
    "transform_weight",
    "components",
    "shrink_ref",
    "ctx_mean",
    "w_cache",
    "floor_ratio",
    "ord_fit_member",
    "ord_w2q",
    "ord_w2m",
    "ord_direct",
    "ord_bound",
];

/// One candidate's prediction.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Prediction {
    pub p_pass: f64,
    pub usage: [f64; LEGS],
}

#[derive(Debug)]
struct Orderer {
    axes_count: usize,
    hidden: usize,
    pool: usize,
    knots: usize,
    mean: Vec<f64>,
    axes: Vec<f64>,
    center: Vec<f64>,
    spread: Vec<f64>,
    w1: Vec<f64>,
    e1: Vec<f64>,
    b1: Vec<f64>,
    w2: Vec<f64>,
    level: Vec<f64>,
    b: Vec<f64>,
    seen: Vec<bool>,
    pool_glm: Vec<f64>,
    pool_net: Vec<f64>,
    pool_n: Vec<i64>,
    alpha: f64,
    recal_x: Vec<f64>,
    recal_c: Vec<f64>,
    recal_n: Vec<i64>,
}

#[derive(Debug)]
pub struct StageOneHead {
    members: usize,
    candidates: usize,
    dim: usize,
    rank: usize,
    hidden: usize,
    embed_dim: usize,
    regimes: usize,
    mean: Vec<f64>,
    scale: Vec<f64>,
    logit_bias: Vec<f64>,
    regime: Vec<f64>,
    difficulty: Vec<f64>,
    v: Vec<f64>,
    u: Vec<f64>,
    w1: Vec<f64>,
    c1: Vec<f64>,
    w2: Vec<f64>,
    c2: Vec<f64>,
    embed: Vec<f64>,
    p_ok: Vec<bool>,
    w_usage: Vec<f64>,
    b_usage: Vec<f64>,
    smear: Vec<f64>,
    low: Vec<f64>,
    high: Vec<f64>,
    usage_ok: Vec<bool>,
    orderer: Orderer,
    ens_point: Vec<usize>,
    cal_platt: Vec<bool>,
    cal_a: Vec<f64>,
    cal_b: Vec<f64>,
    /// Candidates seen only in user traffic.
    pub auxiliary: Vec<bool>,
}

fn dot(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// The index after the last element of `sorted` that is not above `value`:
/// the search `torch.searchsorted(..., right=True)` runs, over the whole
/// padded row.
fn upper_bound(sorted: &[f64], value: f64) -> usize {
    let (mut start, mut end) = (0, sorted.len());
    while start < end {
        let mid = start + ((end - start) >> 1);
        // Written so that a NaN element is stepped over, as in the reference.
        if sorted[mid] > value {
            end = mid;
        } else {
            start = mid + 1;
        }
    }
    start
}

/// NumPy's linear quantile of `pool[..n]` at `q`.
fn linear_quantile(pool: &[f64], n: i64, q: f64) -> f64 {
    let last = (n - 1).max(0) as usize;
    let virtual_index = (n - 1) as f64 * q;
    let below = virtual_index.floor();
    let previous = (below.max(0.0) as usize).min(last);
    let following = (previous + 1).min(last);
    let (a, b) = (pool[previous], pool[following]);
    let gamma = virtual_index - below;
    let diff = b - a;
    if gamma >= 0.5 {
        b - diff * (1.0 - gamma)
    } else {
        a + diff * gamma
    }
}

/// The orderer's ratio for one candidate and leg: 1 where either value is
/// unknown or the pool is thin.
fn order_ratio(glm: f64, net: f64, pool_glm: &[f64], pool_net: &[f64], n: i64, alpha: f64) -> f64 {
    let finite = glm.is_finite() && net.is_finite();
    if !finite || n < MIN_POOL {
        return 1.0;
    }
    let count = n.max(1) as f64;
    let u = upper_bound(pool_net, net) as f64 / count;
    let v = upper_bound(pool_glm, glm) as f64 / count;
    let top = linear_quantile(pool_glm, n, u.clamp(0.0, 1.0)).max(FLOOR_TOKENS);
    let bottom = linear_quantile(pool_glm, n, v.clamp(0.0, 1.0)).max(FLOOR_TOKENS);
    (top / bottom).powf(alpha)
}

/// The length of a tensor that must be one-dimensional.
fn one_dimensional(shape: &[usize]) -> Result<usize, TensorError> {
    match shape {
        [length] => Ok(*length),
        _ => Err(TensorError("ens_point is not one-dimensional".into())),
    }
}

/// `numpy.interp` of `x` on `xp[..n]` / `fp[..n]`, constant beyond the end
/// knots.
fn interp(x: f64, xp: &[f64], fp: &[f64], n: i64) -> f64 {
    let first = fp[0];
    if n <= 1 {
        return first;
    }
    let last = (n - 1) as usize;
    // Only the first `n` knots: the rest of the row is padding.
    let right = upper_bound(&xp[..(last + 1).min(xp.len())], x);
    let j = right.saturating_sub(1).min(last - 1);
    let j1 = (j + 1).min(last);
    let (x0, x1, f0, f1) = (xp[j], xp[j1], fp[j], fp[j1]);
    let span = if x1 > x0 { x1 - x0 } else { 1.0 };
    let inner = (f1 - f0) / span * (x - x0) + f0;
    if x < xp[0] {
        first
    } else if x >= xp[last] {
        fp[last]
    } else {
        inner
    }
}

/// `sigmoid(a logit(clip(p)) + b)` where the map is on and p is known.
fn platt(p: f64, on: bool, a: f64, b: f64) -> f64 {
    if !on || !p.is_finite() {
        return p;
    }
    let q = p.clamp(PLATT_EPS, 1.0 - PLATT_EPS);
    let z = a * (q / (1.0 - q)).ln() + b;
    // exp(-log(1 + exp(-z))), computed without overflow.
    let log_add = (-z).max(0.0) + (-z.abs()).exp().ln_1p();
    (-log_add).exp()
}

fn sigmoid(x: f64) -> f64 {
    1.0 / (1.0 + (-x).exp())
}

impl StageOneHead {
    /// Reads the head from the release's `stage_one.*` tensors.
    pub fn load(tensors: &Tensors) -> Result<Self, TensorError> {
        if let Some(name) = UNSUPPORTED.iter().find(|name| tensors.contains(name)) {
            return Err(TensorError(format!(
                "the stage-one head has a part this crate does not compute ({name})"
            )));
        }
        let &[members, candidates] = tensors.shape("logit_bias")? else {
            return Err(TensorError("logit_bias is not members x candidates".into()));
        };
        let &[dim] = tensors.shape("mean")? else {
            return Err(TensorError("mean is not a vector".into()));
        };
        let &[_, rank, _] = tensors.shape("v")? else {
            return Err(TensorError("v is not members x rank x width".into()));
        };
        let &[_, hidden] = tensors.shape("c1")? else {
            return Err(TensorError("c1 is not members x hidden".into()));
        };
        let &[_, embed_dim] = tensors.shape("c2")? else {
            return Err(TensorError("c2 is not members x embed".into()));
        };
        let &[_, regimes] = tensors.shape("regime")? else {
            return Err(TensorError("regime is not members x regimes".into()));
        };
        let &[ord_legs, ord_axes] = tensors.shape("ord_center")? else {
            return Err(TensorError("ord_center is not legs x axes".into()));
        };
        let &[_, ord_hidden] = tensors.shape("ord_b1")? else {
            return Err(TensorError("ord_b1 is not legs x hidden".into()));
        };
        let &[_, _, pool] = tensors.shape("ord_pool_glm")? else {
            return Err(TensorError(
                "ord_pool_glm is not candidates x legs x pool".into(),
            ));
        };
        let &[_, _, knots] = tensors.shape("ord_recal_x")? else {
            return Err(TensorError(
                "ord_recal_x is not candidates x legs x knots".into(),
            ));
        };
        if ord_legs != LEGS || members == 0 || candidates == 0 || pool == 0 || knots == 0 {
            return Err(TensorError("the stage-one head has an empty axis".into()));
        }
        if tensors.shape("ens_draws")? != [0] {
            return Err(TensorError(
                "the stage-one head carries bootstrap draws, which this crate does not score"
                    .into(),
            ));
        }
        let (m, c, l) = (members, candidates, LEGS);
        let ens_point: Vec<usize> = tensors
            .i64(
                "ens_point",
                &[one_dimensional(tensors.shape("ens_point")?)?],
            )?
            .into_iter()
            .map(|index| usize::try_from(index).ok().filter(|i| *i < m))
            .collect::<Option<_>>()
            .ok_or_else(|| TensorError("ens_point names a member outside the head".into()))?;
        let points = ens_point.len();
        if points == 0 {
            return Err(TensorError("the ensemble has no members".into()));
        }
        let orderer = Orderer {
            axes_count: ord_axes,
            hidden: ord_hidden,
            pool,
            knots,
            mean: tensors.f64("ord_mean", &[dim])?,
            axes: tensors.f64("ord_axes", &[ord_axes, dim])?,
            center: tensors.f64("ord_center", &[l, ord_axes])?,
            spread: tensors.f64("ord_spread", &[l, ord_axes])?,
            w1: tensors.f64("ord_w1", &[l, ord_axes, ord_hidden])?,
            e1: tensors.f64("ord_e1", &[l, c, ord_hidden])?,
            b1: tensors.f64("ord_b1", &[l, ord_hidden])?,
            w2: tensors.f64("ord_w2", &[l, ord_hidden])?,
            level: tensors.f64("ord_level", &[l, c])?,
            b: tensors.f64("ord_b", &[l])?,
            seen: tensors.flags("ord_seen", &[l, c])?,
            pool_glm: tensors.f64("ord_pool_glm", &[c, l, pool])?,
            pool_net: tensors.f64("ord_pool_net", &[c, l, pool])?,
            pool_n: tensors.i64("ord_pool_n", &[c, l])?,
            alpha: tensors.f64("ord_alpha", &[1])?[0],
            recal_x: tensors.f64("ord_recal_x", &[c, l, knots])?,
            recal_c: tensors.f64("ord_recal_c", &[c, l, knots])?,
            recal_n: tensors.i64("ord_recal_n", &[c, l])?,
        };
        if orderer.pool_n.iter().any(|n| *n < 0 || *n as usize > pool)
            || orderer
                .recal_n
                .iter()
                .any(|n| *n < 0 || *n as usize > knots)
        {
            return Err(TensorError("an orderer pool is longer than its row".into()));
        }
        Ok(Self {
            members,
            candidates,
            dim,
            rank,
            hidden,
            embed_dim,
            regimes,
            mean: tensors.f64("mean", &[dim])?,
            scale: tensors.f64("scale", &[dim])?,
            logit_bias: tensors.f64("logit_bias", &[m, c])?,
            regime: tensors.f64("regime", &[m, regimes])?,
            difficulty: tensors.f64("difficulty", &[m, dim])?,
            v: tensors.f64("v", &[m, rank, dim])?,
            u: tensors.f64("u", &[m, c, rank])?,
            w1: tensors.f64("w1", &[m, dim, hidden])?,
            c1: tensors.f64("c1", &[m, hidden])?,
            w2: tensors.f64("w2", &[m, hidden, embed_dim])?,
            c2: tensors.f64("c2", &[m, embed_dim])?,
            embed: tensors.f64("embed", &[m, c, embed_dim])?,
            p_ok: tensors.flags("p_ok", &[m, c])?,
            w_usage: tensors.f64("w_usage", &[m, c, l, dim])?,
            b_usage: tensors.f64("b_usage", &[m, c, l])?,
            smear: tensors.f64("smear", &[m, c, l])?,
            low: tensors.f64("low", &[m, c, l])?,
            high: tensors.f64("high", &[m, c, l])?,
            usage_ok: tensors.flags("usage_ok", &[m, c, l])?,
            orderer,
            ens_point,
            cal_platt: tensors.flags("cal_platt", &[points, c])?,
            cal_a: tensors.f64("cal_a", &[points, c])?,
            cal_b: tensors.f64("cal_b", &[points, c])?,
            auxiliary: tensors.flags("auxiliary", &[c])?,
        })
    }

    pub fn candidates(&self) -> usize {
        self.candidates
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    pub fn regimes(&self) -> usize {
        self.regimes
    }

    /// Every candidate's prediction for one readout under one regime.
    pub fn predict(&self, readout: &[f32], regime: usize) -> Result<Vec<Prediction>, TensorError> {
        if readout.len() != self.dim || regime >= self.regimes {
            return Err(TensorError(
                "the readout or regime does not fit the stage-one head".into(),
            ));
        }
        let (m_count, c_count, d) = (self.members, self.candidates, self.dim);
        let z: Vec<f64> = readout
            .iter()
            .zip(&self.mean)
            .zip(&self.scale)
            .map(|((x, mean), scale)| (f64::from(*x) - mean) * scale)
            .collect();
        let net = self.orderer_net(&z);

        // Per member: p[c] and the ordered usage[c][leg].
        let mut p = vec![f64::NAN; m_count * c_count];
        let mut usage = vec![f64::NAN; m_count * c_count * LEGS];
        for m in 0..m_count {
            let difficulty = dot(&z, &self.difficulty[m * d..(m + 1) * d]);
            let factors: Vec<f64> = (0..self.rank)
                .map(|r| {
                    dot(
                        &z,
                        &self.v[(m * self.rank + r) * d..(m * self.rank + r + 1) * d],
                    )
                })
                .collect();
            let mut hidden = self.c1[m * self.hidden..(m + 1) * self.hidden].to_vec();
            for (k, zk) in z.iter().enumerate() {
                let row = &self.w1[(m * d + k) * self.hidden..(m * d + k + 1) * self.hidden];
                for (h, w) in hidden.iter_mut().zip(row) {
                    *h += zk * w;
                }
            }
            let mut projected = self.c2[m * self.embed_dim..(m + 1) * self.embed_dim].to_vec();
            for (h, value) in hidden.iter().enumerate() {
                if *value > 0.0 {
                    let row = &self.w2[(m * self.hidden + h) * self.embed_dim
                        ..(m * self.hidden + h + 1) * self.embed_dim];
                    for (e, w) in projected.iter_mut().zip(row) {
                        *e += value * w;
                    }
                }
            }
            for c in 0..c_count {
                let mc = m * c_count + c;
                if self.p_ok[mc] {
                    let interaction = dot(&factors, &self.u[mc * self.rank..(mc + 1) * self.rank]);
                    let embed = dot(
                        &projected,
                        &self.embed[mc * self.embed_dim..(mc + 1) * self.embed_dim],
                    );
                    let eta = self.logit_bias[mc] + self.regime[m * self.regimes + regime]
                        - difficulty
                        + interaction
                        + embed;
                    p[mc] = sigmoid(eta.clamp(-40.0, 40.0));
                }
                for leg in 0..LEGS {
                    let i = mc * LEGS + leg;
                    if !self.usage_ok[i] {
                        continue;
                    }
                    let exponent = dot(&z, &self.w_usage[i * d..(i + 1) * d]) + self.b_usage[i];
                    let glm = (exponent.exp() * self.smear[i] - 1.0)
                        .max(self.low[i])
                        .min(self.high[i]);
                    usage[i] = self.ordered(glm, net[leg * c_count + c], c, leg);
                }
            }
        }

        // The calibrated mean over the members.
        let points = self.ens_point.len();
        Ok((0..c_count)
            .map(|c| {
                let (mut total, mut count) = (0.0, 0usize);
                let mut legs = [0.0; LEGS];
                for (i, member) in self.ens_point.iter().enumerate() {
                    let mapped = platt(
                        p[member * c_count + c],
                        self.cal_platt[i * c_count + c],
                        self.cal_a[i * c_count + c],
                        self.cal_b[i * c_count + c],
                    );
                    if mapped.is_finite() {
                        total += mapped;
                        count += 1;
                    }
                    for (leg, sum) in legs.iter_mut().enumerate() {
                        *sum += usage[(member * c_count + c) * LEGS + leg];
                    }
                }
                Prediction {
                    p_pass: if count > 0 {
                        total / count as f64
                    } else {
                        f64::NAN
                    },
                    usage: legs.map(|sum| sum / points as f64),
                }
            })
            .collect())
    }

    /// The orderer network's tokens per leg and candidate (`[leg][candidate]`),
    /// `NaN` where it saw no such rows.
    fn orderer_net(&self, z: &[f64]) -> Vec<f64> {
        let o = &self.orderer;
        let (c_count, d) = (self.candidates, self.dim);
        let centered: Vec<f64> = z.iter().zip(&o.mean).map(|(z, mean)| z - mean).collect();
        let projected: Vec<f64> = (0..o.axes_count)
            .map(|j| dot(&centered, &o.axes[j * d..(j + 1) * d]))
            .collect();
        let mut net = vec![f64::NAN; LEGS * c_count];
        for leg in 0..LEGS {
            let mut shared = o.b1[leg * o.hidden..(leg + 1) * o.hidden].to_vec();
            for (j, value) in projected.iter().enumerate() {
                let x =
                    (value - o.center[leg * o.axes_count + j]) / o.spread[leg * o.axes_count + j];
                let row = &o.w1
                    [(leg * o.axes_count + j) * o.hidden..(leg * o.axes_count + j + 1) * o.hidden];
                for (h, w) in shared.iter_mut().zip(row) {
                    *h += x * w;
                }
            }
            let w2 = &o.w2[leg * o.hidden..(leg + 1) * o.hidden];
            for c in 0..c_count {
                let lc = leg * c_count + c;
                if !o.seen[lc] {
                    continue;
                }
                let e1 = &o.e1[lc * o.hidden..(lc + 1) * o.hidden];
                let eta: f64 = shared
                    .iter()
                    .zip(e1)
                    .zip(w2)
                    .map(|((h, e), w)| (h + e).max(0.0) * w)
                    .sum::<f64>()
                    + o.level[lc]
                    + o.b[leg];
                net[lc] = eta.exp();
            }
        }
        net
    }

    /// One member's usage for a candidate and leg after the orderer and its
    /// recalibration.
    fn ordered(&self, glm: f64, net: f64, candidate: usize, leg: usize) -> f64 {
        let o = &self.orderer;
        let cl = candidate * LEGS + leg;
        let out = glm
            * order_ratio(
                glm,
                net,
                &o.pool_glm[cl * o.pool..(cl + 1) * o.pool],
                &o.pool_net[cl * o.pool..(cl + 1) * o.pool],
                o.pool_n[cl],
                o.alpha,
            );
        if !(out.is_finite() && out > 0.0 && o.recal_n[cl] > 0) {
            return out;
        }
        let log = out.ln();
        let shift = interp(
            log,
            &o.recal_x[cl * o.knots..(cl + 1) * o.knots],
            &o.recal_c[cl * o.knots..(cl + 1) * o.knots],
            o.recal_n[cl],
        );
        (log + shift).exp()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upper_bound_counts_elements_not_above() {
        let row = [1.0, 2.0, 2.0, 5.0, f64::INFINITY, f64::INFINITY];
        assert_eq!(upper_bound(&row, 0.0), 0);
        assert_eq!(upper_bound(&row, 2.0), 3);
        assert_eq!(upper_bound(&row, 4.0), 3);
        assert_eq!(upper_bound(&row, 9.0), 4);
    }

    #[test]
    fn linear_quantile_matches_numpy() {
        // numpy.quantile([1, 2, 4, 8], q) for q in 0, 0.25, 0.5, 0.9, 1.
        let pool = [1.0, 2.0, 4.0, 8.0, f64::INFINITY];
        let cases = [(0.0, 1.0), (0.25, 1.75), (0.5, 3.0), (0.9, 6.8), (1.0, 8.0)];
        for (q, want) in cases {
            assert!((linear_quantile(&pool, 4, q) - want).abs() < 1e-12, "{q}");
        }
    }

    #[test]
    fn interp_is_constant_beyond_the_end_knots() {
        let (xp, fp) = ([1.0, 2.0, 4.0, 0.0], [10.0, 20.0, 40.0, 0.0]);
        assert_eq!(interp(0.5, &xp, &fp, 3), 10.0);
        assert_eq!(interp(1.5, &xp, &fp, 3), 15.0);
        assert_eq!(interp(3.0, &xp, &fp, 3), 30.0);
        assert_eq!(interp(4.0, &xp, &fp, 3), 40.0);
        assert_eq!(interp(9.0, &xp, &fp, 3), 40.0);
        assert_eq!(interp(9.0, &xp, &fp, 1), 10.0);
        // Padding after the `n` knots is never searched.
        let (xp, fp) = (
            [1.0, 2.0, 4.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            [10.0, 20.0, 100.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        );
        assert_eq!(interp(1.5, &xp, &fp, 3), 15.0);
        assert_eq!(interp(3.0, &xp, &fp, 3), 60.0);
    }

    #[test]
    fn a_thin_pool_or_an_unknown_value_leaves_usage_as_predicted() {
        let pool: Vec<f64> = (1..=30).map(f64::from).collect();
        assert_eq!(order_ratio(5.0, 7.0, &pool, &pool, 19, 0.25), 1.0);
        assert_eq!(order_ratio(5.0, f64::NAN, &pool, &pool, 30, 0.25), 1.0);
        // Equal pools: the net value's rank reads the same quantile back.
        let ratio = order_ratio(10.0, 20.0, &pool, &pool, 30, 1.0);
        let want =
            linear_quantile(&pool, 30, 20.0 / 30.0) / linear_quantile(&pool, 30, 10.0 / 30.0);
        assert!((ratio - want).abs() < 1e-12);
    }

    #[test]
    fn platt_leaves_unknown_and_unmapped_values() {
        assert!(platt(f64::NAN, true, 1.0, 0.0).is_nan());
        assert_eq!(platt(0.3, false, 2.0, 1.0), 0.3);
        assert!((platt(0.3, true, 1.0, 0.0) - 0.3).abs() < 1e-12);
        assert!((platt(0.5, true, 1.0, 40.0) - 1.0).abs() < 1e-12);
    }
}
