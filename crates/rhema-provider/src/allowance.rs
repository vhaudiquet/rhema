//! The subscription allowance an adapter reports for its account: what
//! is left of the plan, shaped for a status line. Adapters own the
//! figures — the unit and the plan's vocabulary are theirs alone to
//! know — and consumers print them as given.

/// One provider's subscription allowance: the remaining spend, with its
/// unit, and what the unit is. Dollars on prepaid credit, metered
/// requests on cycle-billed plans.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Allowance {
    /// What the account may still spend, unit included: `"$4.12"`,
    /// `"238/300"`.
    pub amount: String,
    /// What the unit is, one word: `"credit"` for prepaid balance,
    /// `"premium"` for a cycle's metered requests.
    pub kind: String,
}
