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
}

pub fn subtotal(lines: &[Line]) -> f64 {
    let mut sum = 0.0;
    for line in lines {
        sum += line.price * line.quantity as f64;
    }
    sum
}

pub fn tax(amount: f64) -> f64 {
    amount * TAX_RATE
}

pub fn total(lines: &[Line]) -> f64 {
    let amount = subtotal(lines);
    amount + tax(amount)
}

pub fn label(kind: Kind) -> &'static str {
    match kind {
        Kind::Cheque => "Chèque",
        Kind::Transfer => "Virement",
        Kind::Card => "Carte",
    }
}

pub fn round(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

impl fmt::Display for Line {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{} x{} : {:.2}", self.label, self.quantity, self.price)
    }
}
