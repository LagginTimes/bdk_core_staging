use core::{
    cmp::Ordering,
    fmt::{Debug, Display},
    iter::Sum,
    ops::{Add, AddAssign, Sub, SubAssign},
};

use crate::{
    collections::{BTreeSet, HashMap},
    Vec,
};
use bitcoin::{LockTime, Transaction, TxOut};

/// Txin "base" fields include `outpoint` (32+4) and `nSequence` (4). This does not include
/// `scriptSigLen` or `scriptSig`.
pub const TXIN_BASE_WEIGHT: u32 = (32 + 4 + 4) * 4;

/// [`CoinSelector`] is responsible for selecting and deselecting from a set of canididates.
#[derive(Debug, Clone)]
pub struct CoinSelector<'a> {
    candidates: &'a Vec<WeightedValue>,
    selected: BTreeSet<usize>,
    opts: &'a CoinSelectorOpt,
}

/// A [`WeightedValue`] represents an input candidate for [`CoinSelector`]. This can either be a
/// single UTXO, or a group of UTXOs that should be spent together.
#[derive(Debug, Clone, Copy)]
pub struct WeightedValue {
    /// Total value of the UTXO(s) that this [`WeightedValue`] represents.
    pub value: u64,
    /// Total weight of including this/these UTXO(s).
    /// `txin` fields: `prevout`, `nSequence`, `scriptSigLen`, `scriptSig`, `scriptWitnessLen`,
    /// `scriptWitness` should all be included.
    pub weight: u32,
    /// Total number of inputs; so we can calculate extra `varint` weight due to `vin` len changes.
    pub input_count: usize,
    /// Whether this [`WeightedValue`] contains at least one segwit spend.
    pub is_segwit: bool,
}

impl WeightedValue {
    /// Create a new [`WeightedValue`] that represents a single input.
    ///
    /// `satisfaction_weight` is the weight of `scriptSigLen + scriptSig + scriptWitnessLen +
    /// scriptWitness`.
    pub fn new(value: u64, satisfaction_weight: u32, is_segwit: bool) -> WeightedValue {
        let weight = TXIN_BASE_WEIGHT + satisfaction_weight;
        WeightedValue {
            value,
            weight,
            input_count: 1,
            is_segwit,
        }
    }

    /// Effective feerate of this input candidate.
    /// `actual_value - input_weight * feerate`
    pub fn effective_value(&self, opts: &CoinSelectorOpt) -> i64 {
        // we prefer undershooting the candidate's effective value
        self.value as i64 - (self.weight as f32 * opts.target_feerate).ceil() as i64
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CoinSelectorOpt {
    /// The value we need to select.
    pub target_value: u64,
    /// Additional leeway for the target value.
    pub max_extra_target: u64, // TODO: Maybe out of scope here?

    /// The feerate we should try and achieve in sats per weight unit.
    pub target_feerate: f32,
    /// The feerate
    pub long_term_feerate: Option<f32>, // TODO: Maybe out of scope? (waste)
    /// The minimum absolute fee. I.e. needed for RBF.
    pub min_absolute_fee: u64,

    /// The weight of the template transaction including fixed inputs and outputs.
    pub base_weight: u32,
    /// Additional weight if we include the drain (change) output.
    pub drain_weight: u32,
    /// Weight of spending the drain (change) output in the future.
    pub spend_drain_weight: u32, // TODO: Maybe out of scope? (waste)

    /// Minimum value allowed for a drain (change) output.
    pub min_drain_value: u64,
}

impl CoinSelectorOpt {
    fn from_weights(base_weight: u32, drain_weight: u32, spend_drain_weight: u32) -> Self {
        // 0.25 sats/wu == 1 sat/vb
        let target_feerate = 0.25_f32;

        // set `min_drain_value` to dust limit
        let min_drain_value =
            3 * ((drain_weight + spend_drain_weight) as f32 * target_feerate) as u64;

        Self {
            target_value: 0,
            max_extra_target: 0,
            target_feerate,
            long_term_feerate: None,
            min_absolute_fee: 0,
            base_weight,
            drain_weight,
            spend_drain_weight,
            min_drain_value,
        }
    }

    pub fn fund_outputs(
        txouts: &[TxOut],
        drain_output: &TxOut,
        drain_satisfaction_weight: u32,
    ) -> Self {
        let mut tx = Transaction {
            input: vec![],
            version: 1,
            lock_time: LockTime::ZERO.into(),
            output: txouts.to_vec(),
        };
        let base_weight = tx.weight();
        // this awkward calculation is necessary since TxOut doesn't have \.weight()
        let drain_weight = {
            tx.output.push(drain_output.clone());
            tx.weight() - base_weight
        };
        Self {
            target_value: txouts.iter().map(|txout| txout.value).sum(),
            ..Self::from_weights(
                base_weight as u32,
                drain_weight as u32,
                TXIN_BASE_WEIGHT + drain_satisfaction_weight,
            )
        }
    }

    pub fn long_term_feerate(&self) -> f32 {
        self.long_term_feerate.unwrap_or(self.target_feerate)
    }

    pub fn drain_waste(&self) -> i64 {
        (self.drain_weight as f32 * self.target_feerate
            + self.spend_drain_weight as f32 * self.long_term_feerate()) as i64
    }
}

impl<'a> CoinSelector<'a> {
    pub fn candidates(&self) -> &[WeightedValue] {
        &self.candidates
    }

    pub fn candidate(&self, index: usize) -> &WeightedValue {
        &self.candidates[index]
    }

    pub fn new(candidates: &'a Vec<WeightedValue>, opts: &'a CoinSelectorOpt) -> Self {
        Self {
            candidates,
            selected: Default::default(),
            opts,
        }
    }

    pub fn opts(&self) -> &CoinSelectorOpt {
        self.opts
    }

    pub fn select(&mut self, index: usize) {
        assert!(index < self.candidates.len());
        self.selected.insert(index);
    }

    pub fn deselect(&mut self, index: usize) {
        self.selected.remove(&index);
    }

    pub fn is_selected(&self, index: usize) -> bool {
        self.selected.contains(&index)
    }

    pub fn is_empty(&self) -> bool {
        self.selected.is_empty()
    }

    /// Weight sum of all selected inputs.
    pub fn selected_weight(&self) -> u32 {
        self.selected
            .iter()
            .map(|&index| self.candidates[index].weight)
            .sum()
    }

    /// Effective value sum of all selected inputs.
    pub fn selected_effective_value(&self) -> i64 {
        self.selected
            .iter()
            .map(|&index| self.candidates[index].effective_value(&self.opts))
            .sum()
    }

    /// Absolute value sum of all selected inputs.
    pub fn selected_absolute_value(&self) -> u64 {
        self.selected
            .iter()
            .map(|&index| self.candidates[index].value)
            .sum()
    }

    /// Waste sum of all selected inputs.
    pub fn selected_waste(&self) -> i64 {
        (self.selected_weight() as f32 * (self.opts.target_feerate - self.opts.long_term_feerate()))
            as i64
    }

    /// Current weight of template tx + selected inputs.
    pub fn current_weight(&self) -> u32 {
        let witness_header_extra_weight = self
            .selected()
            .find(|(_, wv)| wv.is_segwit)
            .map(|_| 2)
            .unwrap_or(0);
        let vin_count_varint_extra_weight = {
            let input_count = self.selected().map(|(_, wv)| wv.input_count).sum::<usize>();
            (varint_size(input_count) - 1) * 4
        };
        self.opts.base_weight
            + self.selected_weight()
            + witness_header_extra_weight
            + vin_count_varint_extra_weight
    }

    /// Current excess.
    pub fn current_excess(&self) -> i64 {
        let effective_target = self.opts.target_value as i64
            + (self.opts.base_weight as f32 * self.opts.target_feerate) as i64;
        self.selected_effective_value() - effective_target
    }

    /// This is the effective target value.
    pub fn effective_target(&self) -> i64 {
        let (has_segwit, max_input_count) = self
            .candidates()
            .iter()
            .fold((false, 0_usize), |(is_segwit, input_count), c| {
                (is_segwit || c.is_segwit, input_count + c.input_count)
            });

        let effective_base_weight = self.opts.base_weight
            + if has_segwit { 2_u32 } else { 0_u32 }
            + (varint_size(max_input_count) - 1) * 4;

        self.opts.target_value as i64
            + (effective_base_weight as f32 * self.opts.target_feerate).ceil() as i64
    }

    pub fn selected(&self) -> impl Iterator<Item = (usize, &'a WeightedValue)> + '_ {
        self.selected
            .iter()
            .map(|&index| (index, &self.candidates[index]))
    }

    pub fn unselected(&self) -> impl Iterator<Item = (usize, &'a WeightedValue)> + '_ {
        self.candidates
            .iter()
            .enumerate()
            .filter(|(index, _)| !self.selected.contains(index))
    }

    pub fn selected_indexes(&self) -> impl Iterator<Item = usize> + '_ {
        self.selected.iter().cloned()
    }

    pub fn unselected_indexes(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.candidates.len()).filter(|index| !self.selected.contains(index))
    }

    pub fn all_selected(&self) -> bool {
        self.selected.len() == self.candidates.len()
    }

    pub fn select_all(&mut self) {
        self.selected = (0..self.candidates.len()).collect();
    }

    pub fn select_until_finished(&mut self) -> Result<Selection, SelectionFailure> {
        let mut selection = self.finish();

        if selection.is_ok() {
            return selection;
        }

        let unselected = self.unselected_indexes().collect::<Vec<_>>();

        for index in unselected {
            self.select(index);
            selection = self.finish();

            if selection.is_ok() {
                break;
            }
        }

        selection
    }

    pub fn finish(&self) -> Result<Selection, SelectionFailure> {
        let weight_without_drain = self.current_weight();
        let weight_with_drain = weight_without_drain + self.opts.drain_weight;

        let fee_without_drain =
            (weight_without_drain as f32 * self.opts.target_feerate).ceil() as u64;
        let fee_with_drain = (weight_with_drain as f32 * self.opts.target_feerate).ceil() as u64;

        let inputs_minus_outputs = {
            let target_value = self.opts.target_value;
            let selected = self.selected_absolute_value();

            // find the largest unsatisfied constraint (if any), and return error of that constraint
            [
                (
                    SelectionConstraint::TargetValue,
                    target_value.saturating_sub(selected),
                ),
                (
                    SelectionConstraint::TargetFee,
                    (target_value + fee_without_drain).saturating_sub(selected),
                ),
                (
                    SelectionConstraint::MinAbsoluteFee,
                    (target_value + self.opts.min_absolute_fee).saturating_sub(selected),
                ),
            ]
            .into_iter()
            .filter(|&(_, v)| v > 0)
            .max_by_key(|&(_, v)| v)
            .map_or(Ok(()), |(constraint, missing)| {
                Err(SelectionFailure::InsufficientFunds {
                    selected,
                    missing,
                    constraint,
                })
            })?;

            (selected - target_value) as u64
        };

        let fee_without_drain = fee_without_drain.max(self.opts.min_absolute_fee);
        let fee_with_drain = fee_with_drain.max(self.opts.min_absolute_fee);

        let excess_without_drain = inputs_minus_outputs - fee_without_drain;
        let input_waste = self.selected_waste();

        // begin preparing excess strategies for final selection
        let mut excess_strategies = HashMap::new();

        // no drain, excess to fee
        excess_strategies.insert(
            ExcessStrategyKind::ToFee,
            ExcessStrategy {
                recipient_value: self.opts.target_value,
                drain_value: None,
                fee: fee_without_drain + excess_without_drain,
                weight: weight_without_drain,
                waste: input_waste + excess_without_drain as i64,
            },
        );

        // no drain, excess to recipient
        // if `excess == 0`, this result will be the same as the previous, so we don't consider it
        // if `max_extra_target == 0`, there is no leeway for this strategy
        if excess_without_drain > 0 && self.opts.max_extra_target > 0 {
            let extra_recipient_value =
                core::cmp::min(self.opts.max_extra_target, excess_without_drain);
            let extra_fee = excess_without_drain - extra_recipient_value;
            excess_strategies.insert(
                ExcessStrategyKind::ToRecipient,
                ExcessStrategy {
                    recipient_value: self.opts.target_value + extra_recipient_value,
                    drain_value: None,
                    fee: fee_without_drain + extra_fee,
                    weight: weight_without_drain,
                    waste: input_waste + extra_fee as i64,
                },
            );
        }

        // with drain
        if inputs_minus_outputs >= fee_with_drain + self.opts.min_drain_value {
            excess_strategies.insert(
                ExcessStrategyKind::ToDrain,
                ExcessStrategy {
                    recipient_value: self.opts.target_value,
                    drain_value: Some(inputs_minus_outputs.saturating_sub(fee_with_drain)),
                    fee: fee_with_drain,
                    weight: weight_with_drain,
                    waste: input_waste + self.opts.drain_waste(),
                },
            );
        }

        Ok(Selection {
            selected: self.selected.clone(),
            excess: excess_without_drain,
            excess_strategies,
        })
    }
}

#[derive(Clone, Debug)]
pub enum SelectionFailure {
    InsufficientFunds {
        selected: u64,
        missing: u64,
        constraint: SelectionConstraint,
    },
}

impl core::fmt::Display for SelectionFailure {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SelectionFailure::InsufficientFunds {
                selected,
                missing,
                constraint,
            } => write!(
                f,
                "insufficient coins selected; selected={}, missing={}, unsatisfied_constraint={:?}",
                selected, missing, constraint
            ),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for SelectionFailure {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionConstraint {
    /// The target is not met
    TargetValue,
    /// The target fee (given the feerate) is not met
    TargetFee,
    /// Min absolute fee in not met
    MinAbsoluteFee,
}

impl core::fmt::Display for SelectionConstraint {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SelectionConstraint::TargetValue => core::write!(f, "target_value"),
            SelectionConstraint::TargetFee => core::write!(f, "target_fee"),
            SelectionConstraint::MinAbsoluteFee => core::write!(f, "min_absolute_fee"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Selection {
    pub selected: BTreeSet<usize>,
    pub excess: u64,
    pub excess_strategies: HashMap<ExcessStrategyKind, ExcessStrategy>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, core::hash::Hash)]
pub enum ExcessStrategyKind {
    ToFee,
    ToRecipient,
    ToDrain,
}

#[derive(Clone, Copy, Debug)]
pub struct ExcessStrategy {
    pub recipient_value: u64,
    pub drain_value: Option<u64>,
    pub fee: u64,
    pub weight: u32,
    pub waste: i64,
}

impl core::fmt::Display for ExcessStrategyKind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ExcessStrategyKind::ToFee => core::write!(f, "to_fee"),
            ExcessStrategyKind::ToRecipient => core::write!(f, "to_recipient"),
            ExcessStrategyKind::ToDrain => core::write!(f, "to_drain"),
        }
    }
}

impl ExcessStrategy {
    /// Returns feerate in sats/wu.
    pub fn feerate(&self) -> f32 {
        self.fee as f32 / self.weight as f32
    }
}

impl Selection {
    pub fn apply_selection<'a, T>(
        &'a self,
        candidates: &'a [T],
    ) -> impl Iterator<Item = &'a T> + 'a {
        self.selected.iter().map(|i| &candidates[*i])
    }

    /// Returns the [`ExcessStrategy`] that results in the least waste.
    pub fn best_strategy(&self) -> (&ExcessStrategyKind, &ExcessStrategy) {
        self.excess_strategies
            .iter()
            .min_by_key(|&(_, a)| a.waste)
            .expect("selection has no excess strategy")
    }
}

fn varint_size(v: usize) -> u32 {
    if v <= 0xfc {
        return 1;
    }
    if v <= 0xffff {
        return 3;
    }
    if v <= 0xffff_ffff {
        return 5;
    }
    return 9;
}

pub trait BnbNum:
    Display
    + Debug
    + Copy
    + PartialOrd
    + Sum
    + Add<Output = Self>
    + Sub<Output = Self>
    + AddAssign
    + SubAssign
{
    const ZERO: Self;
    const MAX: Self;
}

impl BnbNum for i64 {
    const ZERO: Self = 0;
    const MAX: Self = i64::MAX;
}

impl BnbNum for u64 {
    const ZERO: Self = 0;
    const MAX: Self = u64::MAX;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct CombinedValue {
    pub eff_value: i64,
    pub abs_value: u64,
}

impl Display for CombinedValue {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "(eff: {}, abs: {})", self.eff_value, self.abs_value)
    }
}

impl PartialEq for CombinedValue {
    fn eq(&self, other: &Self) -> bool {
        self.eff_value == other.eff_value && self.abs_value == other.abs_value
    }
}

impl PartialOrd for CombinedValue {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        // equal if both are equal
        if self.eff_value == other.eff_value && self.abs_value == other.abs_value {
            return Some(Ordering::Equal);
        }

        // only greater if both values are greater
        if self.eff_value >= other.eff_value && self.abs_value >= other.abs_value {
            return Some(Ordering::Greater);
        }

        // less if at least one value is lesser
        if self.eff_value < other.eff_value || self.abs_value < other.abs_value {
            return Some(Ordering::Less);
        }

        None
    }
}

impl Sum for CombinedValue {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.fold(Self::default(), |a, b| a + b)
    }
}

impl Add for CombinedValue {
    type Output = Self;

    fn add(self, rhs: Self) -> Self {
        Self {
            eff_value: self.eff_value + rhs.eff_value,
            abs_value: self.abs_value + rhs.abs_value,
        }
    }
}

impl Sub for CombinedValue {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self {
        Self {
            eff_value: self.eff_value - rhs.eff_value,
            abs_value: self.abs_value - rhs.abs_value,
        }
    }
}

impl AddAssign for CombinedValue {
    fn add_assign(&mut self, rhs: Self) {
        self.eff_value += rhs.eff_value;
        self.abs_value += rhs.abs_value;
    }
}

impl SubAssign for CombinedValue {
    fn sub_assign(&mut self, rhs: Self) {
        self.eff_value -= rhs.eff_value;
        self.abs_value -= rhs.abs_value;
    }
}

impl BnbNum for CombinedValue {
    const ZERO: Self = Self {
        eff_value: 0,
        abs_value: 0,
    };
    const MAX: Self = Self {
        eff_value: i64::MAX,
        abs_value: u64::MAX,
    };
}

impl CombinedValue {
    /// Returns the "bounds" for Branch and Bound: `(target_value, upper_bound)`.
    pub fn bounds(selector: &CoinSelector) -> (Self, Self) {
        let opts = selector.opts();
        let target_value = Self {
            eff_value: selector.effective_target(),
            abs_value: opts.target_value + opts.min_absolute_fee,
        };
        let upper_bound = Self {
            eff_value: target_value.eff_value + opts.drain_waste(),
            abs_value: target_value.abs_value
                + (opts.drain_weight as f32 * opts.target_feerate) as u64,
        };
        (target_value, upper_bound)
    }
}

pub struct BnbParams<'c, 'f, V, M> {
    /// Selection pool of candidates
    pub pool: Vec<(usize, &'c WeightedValue)>,

    /// Target value (lower bound)
    pub target_value: V,
    /// Upper bound
    pub upper_bound: V,

    /// Does metric increase with each selection?
    /// For example, the waste metric increases with each selection when long term feerate is lower
    /// than effective feerate
    pub metric_increases: bool,

    /// Calculates the value (`V`) that a single candidate introduces.
    pub value_fn: &'f dyn Fn(&CoinSelector, &WeightedValue) -> V,
    /// Calculates the metric (`M`) that a single candidate introduces.
    pub metric_fn: &'f dyn Fn(&CoinSelector, &WeightedValue) -> M,
    /// Calculates additional metric (`M`) when value sum (`V`) is in range.
    /// I.e. if `M` is the waste metric, this would return the excess.
    pub additional_metric_fn: &'f dyn Fn(&CoinSelector) -> M,
}

pub struct BnbState<'c, 'f, V, M> {
    /// Bnb parameters
    params: &'f BnbParams<'c, 'f, V, M>,
    /// Current selection
    selection: CoinSelector<'c>,
    /// Records the metric value of the best selection, `M` is the metric to minimize
    best: Option<M>,

    /// Position within the selection pool
    pos: usize,
    /// Whether we have exhausted all rounds
    done: bool,
    /// Remaining effective value of the current branch
    remaining_value: V,
}

impl<'c, 'f, V: BnbNum, M: BnbNum> BnbState<'c, 'f, V, M> {
    pub fn new(
        params: &'f BnbParams<'c, 'f, V, M>,
        selector: CoinSelector<'c>,
    ) -> Result<Self, &'static str> {
        let remaining_value = params
            .pool
            .iter()
            .map(|(_, c)| (params.value_fn)(&selector, c))
            .sum::<V>();
        let selected_value = selector
            .selected()
            .map(|(_, c)| (params.value_fn)(&selector, c))
            .sum::<V>();

        if selected_value + remaining_value < params.target_value {
            return Err("remaining value is insufficient");
        }

        Ok(Self {
            params,
            pos: 0,
            done: false,
            remaining_value,
            selection: selector,
            best: None,
        })
    }

    pub fn current_value(&self) -> V {
        self.selection
            .selected()
            .map(|(_, c)| (self.params.value_fn)(&self.selection, c))
            .sum()
    }

    pub fn current_metric(&self) -> M {
        self.selection
            .selected()
            .map(|(_, c)| (self.params.metric_fn)(&self.selection, c))
            .sum()
    }

    pub fn best_metric(&self) -> M {
        self.best.unwrap_or(M::MAX)
    }

    /// Checks current selection, returns `(is_solution, backtrack)`.
    pub fn check(&self) -> (bool, bool) {
        let current_value = self.current_value();

        // is remaining value enough?
        if current_value + self.remaining_value < self.params.target_value {
            return (false, true);
        }

        // is current value above range?
        if current_value > self.params.upper_bound {
            return (false, true);
        }

        // is current value within range?
        if current_value >= self.params.target_value {
            return (true, true);
        }

        // current value is most definitely below range

        // if metric increases with each selection, and current metric already is greater than
        // best metric, selecting more candidates will just result in a worse metric
        if self.params.metric_increases && self.current_metric() > self.best_metric() {
            return (false, true);
        }

        // this should not happen and represents a faulty implementation
        debug_assert!(self.pos < self.params.pool.len());

        // select more
        return (false, false);
    }

    /// Determines whether we can perform the early bailout optimisation.
    ///
    /// If the candidate at the previous position is NOT selected and has the same weight and
    /// value as the current candidate, we can skip selecting the current candidate.
    pub fn early_bailout(&self) -> bool {
        if self.pos > 0 && !self.selection.is_empty() {
            let (_, candidate) = self.params.pool[self.pos];
            let (prev_index, prev_candidate) = self.params.pool[self.pos - 1];

            if !self.selection.is_selected(prev_index)
                && candidate.value == prev_candidate.value
                && candidate.weight == prev_candidate.weight
            {
                return true;
            }
        }

        false
    }
}

impl<'c, 'f, V: BnbNum, M: BnbNum> Iterator for BnbState<'c, 'f, V, M> {
    type Item = Option<CoinSelector<'c>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }

        let (is_solution, backtrack) = self.check();

        // if solution has a better (lower) metric value than the current best, replace the current
        // best and return the new best selection
        let best_selection = {
            let mut best_selection = None;
            if is_solution {
                let current_metric =
                    self.current_metric() + (self.params.additional_metric_fn)(&self.selection);

                if current_metric <= self.best_metric() {
                    self.best.replace(current_metric);
                    best_selection = Some(self.selection.clone());
                }
            }
            best_selection
        };

        if backtrack {
            // find the last `pos` with a selected candidate
            let last = (0..self.pos).rev().find_map(|pos| {
                let (index, candidate) = self.params.pool[pos];

                if self.selection.is_selected(index) {
                    return Some((pos, index));
                }

                self.remaining_value += (self.params.value_fn)(&self.selection, candidate);
                return None;
            });

            match last {
                Some((last_selected_pos, last_selected_index)) => {
                    // deselect last `pos`, next round will check omission branch
                    self.pos = last_selected_pos;
                    self.selection.deselect(last_selected_index);
                }
                None => {
                    // nothing is selected, all solutions searched
                    self.done = true;
                }
            }
        } else {
            let (index, candidate) = self.params.pool[self.pos];
            self.remaining_value -= (self.params.value_fn)(&self.selection, candidate);

            if !self.early_bailout() {
                self.selection.select(index);
            }
        }

        self.pos += 1;

        if best_selection.is_some() || !self.done {
            Some(best_selection)
        } else {
            None
        }
    }
}

/// This is a variation of the Branch and Bound Coin Selection algorithm designed by Murch (as seen
/// in Bitcoin Core).
///
/// The differences are as follows:
/// * In additional to working with effective values, we also work with absolute values.
///   This way, we can use bounds of absolute values to enforce `min_absolute_fee` (which is used by
///   RBF), and `max_extra_target` (which can be used to increase the possible solution set, given
///   that the sender is okay with sending extra to the receiver).
///
/// Murch's Master Thesis: https://murch.one/wp-content/uploads/2016/11/erhardt2016coinselection.pdf
/// Bitcoin Core Implementation: https://github.com/bitcoin/bitcoin/blob/23.x/src/wallet/coinselection.cpp#L65
///
/// TODO: Another optimization we could do is figure out candidate with smallest waste, and
/// if we find a result with waste equal to this, we can just break.
pub fn coin_select_bnb(max_tries: usize, selector: CoinSelector) -> Option<CoinSelector> {
    let opts = selector.opts();

    // prepare pool of candidates to select from:
    // * filter out candidates with negative/zero effective values
    // * sort candidates by descending effective value
    let pool = {
        let mut pool = selector
            .unselected()
            .filter(|(_, c)| c.effective_value(&opts) > 0)
            .collect::<Vec<_>>();
        pool.sort_unstable_by(|(_, a), (_, b)| {
            let a = a.effective_value(&opts);
            let b = b.effective_value(&opts);
            b.cmp(&a)
        });
        pool
    };

    // prepare lower and upper bounds for "value"
    let (target_value, upper_bound) = CombinedValue::bounds(&selector);

    // this calculates "value" for a single candidate
    let value_fn = |selector: &CoinSelector, candidate: &WeightedValue| -> CombinedValue {
        CombinedValue {
            eff_value: candidate.effective_value(&selector.opts),
            abs_value: candidate.value,
        }
    };

    // this calculates "metric" for a single candidate
    let metric_fn = |selector: &CoinSelector, candidate: &WeightedValue| -> i64 {
        let opts = selector.opts();
        (candidate.weight as f32 * (opts.target_feerate - opts.long_term_feerate())) as i64
    };

    // this calculates additional "metric", when "value" sum is within lower and upper bounds
    let additional_metric_fn = |selector: &CoinSelector| -> i64 {
        selector.selected_effective_value() - target_value.eff_value
    };

    let params = BnbParams {
        pool,
        target_value,
        upper_bound,
        metric_increases: opts.target_feerate > opts.long_term_feerate(),
        value_fn: &value_fn,
        metric_fn: &metric_fn,
        additional_metric_fn: &additional_metric_fn,
    };

    let state = BnbState::new(&params, selector).ok()?;
    state
        .take(max_tries)
        .reduce(|b, c| if c.is_some() { c } else { b })?
}

#[cfg(feature = "std")]
pub mod evaluate_cs {
    use super::{CoinSelector, ExcessStrategyKind, Selection, Vec};

    pub fn evaluate<F>(
        initial_selector: CoinSelector,
        mut select: F,
    ) -> Result<Evaluation, EvaluationFailure>
    where
        F: FnMut(&mut CoinSelector) -> bool,
    {
        let mut selector = initial_selector.clone();
        let start_time = std::time::SystemTime::now();
        let has_solution = select(&mut selector);
        let elapsed = start_time.elapsed().expect("system time error");

        if has_solution {
            let solution = selector.finish().expect("failed to finish what we started");

            let elapsed_per_candidate = elapsed / selector.candidates.len() as _;

            let waste_vec = solution
                .excess_strategies
                .iter()
                .map(|(_, s)| s.waste)
                .collect::<Vec<_>>();

            let waste_mean = waste_vec.iter().sum::<i64>() as f32 / waste_vec.len() as f32;
            let waste_median = if waste_vec.len() % 2 != 0 {
                waste_vec[waste_vec.len() / 2] as f32
            } else {
                (waste_vec[(waste_vec.len() - 1) / 2] + waste_vec[waste_vec.len() / 2]) as f32 / 2.0
            };

            Ok(Evaluation {
                initial_selector,
                solution,
                elapsed,
                elapsed_per_candidate,
                waste_median,
                waste_mean,
            })
        } else {
            Err(EvaluationFailure {
                initial: initial_selector,
                elapsed,
            })
        }
    }

    #[derive(Debug, Clone)]
    pub struct Evaluation<'a> {
        pub initial_selector: CoinSelector<'a>,
        pub solution: Selection,

        pub elapsed: std::time::Duration,
        pub elapsed_per_candidate: std::time::Duration,

        pub waste_median: f32,
        pub waste_mean: f32,
    }

    impl<'a> Evaluation<'a> {
        pub fn waste(&self, strategy_kind: ExcessStrategyKind) -> i64 {
            self.solution.excess_strategies[&strategy_kind].waste
        }

        pub fn feerate_offset(&self, strategy_kind: ExcessStrategyKind) -> f32 {
            let target_rate = self.initial_selector.opts.target_feerate;
            let actual_rate = self.solution.excess_strategies[&strategy_kind].feerate();
            actual_rate - target_rate
        }
    }

    impl<'a> core::fmt::Display for Evaluation<'a> {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            writeln!(f, "Evaluation:")?;
            writeln!(
                f,
                "\t* Candidates: {}",
                self.initial_selector.candidates.len()
            )?;
            writeln!(
                f,
                "\t* Initial selection: {}",
                self.initial_selector.selected.len()
            )?;
            writeln!(f, "\t* Final selection: {}", self.solution.selected.len())?;
            writeln!(f, "\t* Elapsed: {:?}", self.elapsed)?;
            writeln!(
                f,
                "\t* Elapsed per candidate: {:?}",
                self.elapsed_per_candidate
            )?;
            Ok(())
        }
    }

    #[derive(Debug, Clone)]
    pub struct EvaluationFailure<'a> {
        initial: CoinSelector<'a>,
        elapsed: std::time::Duration,
    }

    impl<'a> core::fmt::Display for EvaluationFailure<'a> {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            write!(
                f,
                "cs algorithm failed to find a solution: elapsed={}s target_feerate={}sats/wu",
                self.elapsed.as_secs(),
                self.initial.opts.target_feerate
            )
        }
    }

    impl<'a> std::error::Error for EvaluationFailure<'a> {}
}

#[cfg(test)]
pub mod tester {
    use super::*;
    use bitcoin::{
        secp256k1::{All, Secp256k1},
        TxOut,
    };
    use miniscript::{
        plan::{Assets, Plan},
        Descriptor, DescriptorPublicKey,
    };

    #[derive(Debug, Clone)]
    pub struct TestCandidate {
        pub txo: TxOut,
        pub plan: Plan<DescriptorPublicKey>,
    }

    impl From<TestCandidate> for WeightedValue {
        fn from(test_candidate: TestCandidate) -> Self {
            Self {
                value: test_candidate.txo.value,
                weight: TXIN_BASE_WEIGHT + test_candidate.plan.expected_weight() as u32,
                input_count: 1,
                is_segwit: test_candidate.plan.witness_version().is_some(),
            }
        }
    }

    pub struct Tester {
        descriptor: Descriptor<DescriptorPublicKey>,
        assets: Assets<DescriptorPublicKey>,
    }

    impl Tester {
        pub fn new(secp: &Secp256k1<All>, desc_str: &str) -> Self {
            // let desc_str = "tr(xprv9uBuvtdjghkz8D1qzsSXS9Vs64mqrUnXqzNccj2xcvnCHPpXKYE1U2Gbh9CDHk8UPyF2VuXpVkDA7fk5ZP4Hd9KnhUmTscKmhee9Dp5sBMK)";
            let (descriptor, seckeys) =
                Descriptor::<DescriptorPublicKey>::parse_descriptor(secp, desc_str).unwrap();

            let assets = Assets {
                keys: seckeys.keys().cloned().collect(),
                ..Default::default()
            };

            Self { descriptor, assets }
        }

        pub fn gen_candidate(&self, derivation_index: u32, value: u64) -> TestCandidate {
            let descriptor = self.descriptor.at_derivation_index(derivation_index);
            let plan = descriptor.plan_satisfaction(&self.assets).unwrap();
            let txo = TxOut {
                value,
                script_pubkey: descriptor.script_pubkey(),
            };
            TestCandidate { txo, plan }
        }

        pub fn gen_weighted_value(&self, value: u64) -> WeightedValue {
            self.gen_candidate(0, value).into()
        }

        pub fn gen_weighted_values(&self, out: &mut Vec<WeightedValue>, count: usize, value: u64) {
            (0..count).for_each(|_| out.push(self.gen_candidate(0, value).into()))
        }

        pub fn gen_opts(&self, recipient_value: u64) -> CoinSelectorOpt {
            let recipient = self.gen_candidate(0, recipient_value);
            let drain = self.gen_candidate(0, 0);
            CoinSelectorOpt::fund_outputs(
                &[recipient.txo],
                &drain.txo,
                drain.plan.expected_weight() as u32,
            )
        }
    }
}

#[cfg(test)]
mod test_bnb {
    use bitcoin::secp256k1::Secp256k1;

    use crate::coin_select::{evaluate_cs::evaluate, ExcessStrategyKind};

    use super::{
        coin_select_bnb,
        evaluate_cs::{Evaluation, EvaluationFailure},
        tester::Tester,
        CoinSelector, CoinSelectorOpt, Vec, WeightedValue,
    };

    fn tester() -> Tester {
        const DESC_STR: &str = "tr(xprv9uBuvtdjghkz8D1qzsSXS9Vs64mqrUnXqzNccj2xcvnCHPpXKYE1U2Gbh9CDHk8UPyF2VuXpVkDA7fk5ZP4Hd9KnhUmTscKmhee9Dp5sBMK)";
        Tester::new(&Secp256k1::default(), DESC_STR)
    }

    fn evaluate_bnb(
        initial_selector: CoinSelector,
        max_tries: usize,
    ) -> Result<Evaluation, EvaluationFailure> {
        evaluate(initial_selector, |cs| {
            coin_select_bnb(max_tries, cs.clone()).map_or(false, |new_cs| {
                *cs = new_cs;
                true
            })
        })
    }

    #[test]
    fn not_enough_coins() {
        let t = tester();
        let candidates: Vec<WeightedValue> = vec![
            t.gen_candidate(0, 100_000).into(),
            t.gen_candidate(1, 100_000).into(),
        ];
        let opts = t.gen_opts(200_000);
        let selector = CoinSelector::new(&candidates, &opts);
        // assert!(!coin_select_bnb(10_000, &mut selector));
        assert!(!coin_select_bnb(10_000, selector).is_some());
    }

    #[test]
    fn exactly_enough_coins_preselected() {
        let t = tester();
        let candidates: Vec<WeightedValue> = vec![
            t.gen_candidate(0, 100_000).into(), // to preselect
            t.gen_candidate(1, 100_000).into(), // to preselect
            t.gen_candidate(2, 100_000).into(),
        ];
        let opts = CoinSelectorOpt {
            target_feerate: 0.0,
            ..t.gen_opts(200_000)
        };
        let selector = {
            let mut selector = CoinSelector::new(&candidates, &opts);
            selector.select(0); // preselect
            selector.select(1); // preselect
            selector
        };

        let evaluation = evaluate_bnb(selector, 10_000).expect("eval failed");
        println!("{}", evaluation);
        assert_eq!(evaluation.solution.selected, (0..=1).collect());
        assert_eq!(evaluation.solution.excess_strategies.len(), 1);
        assert_eq!(
            evaluation.feerate_offset(ExcessStrategyKind::ToFee).floor(),
            0.0
        );
    }

    /// `cost_of_change` acts as the upper-bound in Bnb, we check whether these boundaries are
    /// enforced in code
    #[test]
    fn cost_of_change() {
        let t = tester();
        let candidates: Vec<WeightedValue> = vec![
            t.gen_candidate(0, 200_000).into(),
            t.gen_candidate(1, 200_000).into(),
            t.gen_candidate(2, 200_000).into(),
        ];

        // lowest and highest possible `recipient_value` opts for derived `drain_waste`, assuming
        // that we want 2 candidates selected
        let (lowest_opts, highest_opts) = {
            let opts = t.gen_opts(0);

            let fee_from_inputs =
                (candidates[0].weight as f32 * opts.target_feerate).ceil() as u64 * 2;
            let fee_from_template =
                ((opts.base_weight + 2) as f32 * opts.target_feerate).ceil() as u64;

            let lowest_opts = CoinSelectorOpt {
                target_value: 400_000 + 1
                    - fee_from_inputs
                    - fee_from_template
                    - opts.drain_waste() as u64,
                ..opts
            };

            let highest_opts = CoinSelectorOpt {
                target_value: 400_000 - fee_from_inputs - fee_from_template,
                ..opts
            };

            (lowest_opts, highest_opts)
        };

        // test lowest possible target we are able to select
        let lowest_eval = evaluate_bnb(CoinSelector::new(&candidates, &lowest_opts), 10_000);
        assert!(lowest_eval.is_ok());
        let lowest_eval = lowest_eval.unwrap();
        println!("LB {}", lowest_eval);
        assert_eq!(lowest_eval.solution.selected.len(), 2);
        assert_eq!(lowest_eval.solution.excess_strategies.len(), 1);
        assert_eq!(
            lowest_eval
                .feerate_offset(ExcessStrategyKind::ToFee)
                .floor(),
            0.0
        );

        // test highest possible target we are able to select
        let highest_eval = evaluate_bnb(CoinSelector::new(&candidates, &highest_opts), 10_000);
        assert!(highest_eval.is_ok());
        let highest_eval = highest_eval.unwrap();
        println!("UB {}", highest_eval);
        assert_eq!(highest_eval.solution.selected.len(), 2);
        assert_eq!(highest_eval.solution.excess_strategies.len(), 1);
        assert_eq!(
            highest_eval
                .feerate_offset(ExcessStrategyKind::ToFee)
                .floor(),
            0.0
        );

        // test lower out of bounds
        let loob_opts = CoinSelectorOpt {
            target_value: lowest_opts.target_value - 1,
            ..lowest_opts
        };
        let loob_eval = evaluate_bnb(CoinSelector::new(&candidates, &loob_opts), 10_000);
        assert!(loob_eval.is_err());
        println!("Lower OOB: {}", loob_eval.unwrap_err());

        // test upper out of bounds
        let uoob_opts = CoinSelectorOpt {
            target_value: highest_opts.target_value + 1,
            ..highest_opts
        };
        let uoob_eval = evaluate_bnb(CoinSelector::new(&candidates, &uoob_opts), 10_000);
        assert!(uoob_eval.is_err());
        println!("Upper OOB: {}", uoob_eval.unwrap_err());
    }

    #[test]
    fn try_select() {
        let t = tester();
        let candidates: Vec<WeightedValue> = vec![
            t.gen_candidate(0, 300_000).into(),
            t.gen_candidate(1, 300_000).into(),
            t.gen_candidate(2, 300_000).into(),
            t.gen_candidate(3, 200_000).into(),
            t.gen_candidate(4, 200_000).into(),
        ];
        let make_opts = |v: u64| -> CoinSelectorOpt {
            CoinSelectorOpt {
                target_feerate: 0.0,
                ..t.gen_opts(v)
            }
        };

        let test_cases = vec![
            (make_opts(100_000), false, 0),
            (make_opts(200_000), true, 1),
            (make_opts(300_000), true, 1),
            (make_opts(500_000), true, 2),
            (make_opts(1_000_000), true, 4),
            (make_opts(1_200_000), false, 0),
            (make_opts(1_300_000), true, 5),
            (make_opts(1_400_000), false, 0),
        ];

        for (opts, expect_solution, expect_selected) in test_cases {
            let res = evaluate_bnb(CoinSelector::new(&candidates, &opts), 10_000);
            assert_eq!(res.is_ok(), expect_solution);

            match res {
                Ok(eval) => {
                    println!("{}", eval);
                    assert_eq!(eval.feerate_offset(ExcessStrategyKind::ToFee), 0.0);
                    assert_eq!(eval.solution.selected.len(), expect_selected as _);
                }
                Err(err) => println!("expected failure: {}", err),
            }
        }
    }

    #[test]
    fn early_bailout_optimization() {
        let t = tester();

        // target: 300_000
        // candidates: 2x of 125_000, 1000x of 100_000, 1x of 50_000
        // expected solution: 2x 125_000, 1x 50_000
        // set bnb max tries: 1100, should succeed
        let candidates = {
            let mut candidates: Vec<WeightedValue> = vec![
                t.gen_candidate(0, 125_000).into(),
                t.gen_candidate(1, 125_000).into(),
                t.gen_candidate(2, 50_000).into(),
            ];
            (3..3 + 1000_u32)
                .for_each(|index| candidates.push(t.gen_candidate(index, 100_000).into()));
            candidates
        };
        let opts = CoinSelectorOpt {
            target_feerate: 0.0,
            ..t.gen_opts(300_000)
        };

        let result = evaluate_bnb(CoinSelector::new(&candidates, &opts), 1100);
        assert!(result.is_ok());

        let eval = result.unwrap();
        println!("{}", eval);
        assert_eq!(eval.solution.selected, (0..=2).collect());
    }

    #[test]
    fn should_exhaust_iteration() {
        static MAX_TRIES: usize = 1000;
        let t = tester();
        let candidates = (0..MAX_TRIES + 1)
            .map(|index| t.gen_candidate(index as _, 10_000).into())
            .collect::<Vec<WeightedValue>>();
        let opts = t.gen_opts(10_001 * MAX_TRIES as u64);
        let result = evaluate_bnb(CoinSelector::new(&candidates, &opts), MAX_TRIES);
        assert!(result.is_err());
        println!("error as expected: {}", result.unwrap_err());
    }

    /// Solution should have fee >= min_absolute_fee
    #[test]
    fn min_absolute_fee() {
        let t = tester();
        let candidates = {
            let mut candidates = Vec::new();
            t.gen_weighted_values(&mut candidates, 5, 10_000);
            t.gen_weighted_values(&mut candidates, 5, 20_000);
            t.gen_weighted_values(&mut candidates, 5, 30_000);
            t.gen_weighted_values(&mut candidates, 10, 10_300);
            t.gen_weighted_values(&mut candidates, 10, 10_500);
            t.gen_weighted_values(&mut candidates, 10, 10_700);
            t.gen_weighted_values(&mut candidates, 10, 10_900);
            t.gen_weighted_values(&mut candidates, 10, 11_000);
            t.gen_weighted_values(&mut candidates, 10, 12_000);
            t.gen_weighted_values(&mut candidates, 10, 13_000);
            candidates
        };
        let mut opts = CoinSelectorOpt {
            min_absolute_fee: 1,
            ..t.gen_opts(100_000)
        };

        (1..=120_u64).for_each(|fee_factor| {
            opts.min_absolute_fee = fee_factor * 31;

            let result = evaluate_bnb(CoinSelector::new(&candidates, &opts), 21_000);
            match result {
                Ok(result) => {
                    println!("Solution {}", result);
                    let fee = result.solution.excess_strategies[&ExcessStrategyKind::ToFee].fee;
                    assert!(fee >= opts.min_absolute_fee);
                    assert_eq!(result.solution.excess_strategies.len(), 1);
                }
                Err(err) => {
                    println!("No Solution: {}", err);
                }
            }
        });
    }

    /// TODO: UNIMPLEMENTED TESTS:
    /// * Decreasing feerate -> select less, increasing feerate -> select more
    /// * Excess strategies:
    ///     * We should always have `ExcessStrategy::ToFee`.
    ///     * We should only have `ExcessStrategy::ToRecipient` when `max_extra_target > 0`.
    ///     * We should only have `ExcessStrategy::ToDrain` when `drain_value >= min_drain_value`.
    /// * Fuzz
    ///     * Solution feerate should never be lower than target feerate
    ///     * Solution fee should never be lower than `min_absolute_fee`
    ///     * Preselected should always remain selected
    fn _todo() {}
}
