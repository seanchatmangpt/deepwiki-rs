//! Tiny crate used as a real Rustdoc JSON fixture for semantic documentation.
//!
//! See [`Ledger`] and [`admit`].

/// An append-only ledger of admitted entries.
pub struct Ledger {
    /// Entries in admission order.
    pub entries: Vec<String>,
}

/// Admit `entry` into `ledger`; refuses empty entries. Uses [`Ledger`].
pub fn admit(ledger: &mut Ledger, entry: &str) -> Result<(), Refusal> {
    if entry.is_empty() {
        return Err(Refusal::Empty);
    }
    ledger.entries.push(entry.to_string());
    Ok(())
}

/// Typed refusal returned by [`admit`].
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The entry was empty.
    Empty,
}

pub(crate) fn undocumented_private() {}
