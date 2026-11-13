//! Facturation : calcul des montants d'une facture.

use std::fmt;

pub const TAX_RATE: f64 = 0.20;

#[derive(Debug, Clone, Copy)]
pub enum Kind {
    Cheque,
    Transfer,
    Card,
}

#[derive(Debug, Clone)]
pub struct Line {
    pub label: String,
    pub price: f64,
    pub quantity: u32,
    pub tax_exempt: bool,
}

pub fn subtotal(lines: &[Line]) -> f64 {
    let mut sum = 0.0;
    for line in lines {
        sum += line.price * f64::from(line.quantity);
    }
    sum
}

pub fn tax(amount: f64) -> f64 {
    (amount * TAX_RATE).max(0.0)
}

pub fn total(lines: &[Line]) -> f64 {
    let amount = subtotal(lines);
    amount + tax(amount)
}

/// Total after a percentage discount, never below zero.
pub fn discounted(lines: &[Line], percent: f64) -> f64 {
    (total(lines) * (1.0 - percent / 100.0)).max(0.0)
}

pub fn label(kind: Kind) -> &'static str {
    match kind {
        Kind::Cheque => "Chèque",
        Kind::Transfer => "Virement SEPA",
        Kind::Card => "Carte",
    }
}

pub fn round(value: f64) -> f64 {
    // Au centime le plus proche.
    (value * 100.0).round() / 100.0
}

impl fmt::Display for Line {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{} x{} : {:.2} €", self.label, self.quantity, self.price)
    }
}
