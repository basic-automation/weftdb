use bigdecimal::BigDecimal;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Distance {
    positive: BigDecimal,
    negative: BigDecimal,
}

impl Distance {
    pub fn new(positive: BigDecimal, negative: BigDecimal) -> Self {
        Self { positive, negative }
    }

    pub fn positive(&self) -> &BigDecimal {
        &self.positive
    }

    pub fn negative(&self) -> &BigDecimal {
        &self.negative
    }

    pub fn set_positive(&mut self, positive: BigDecimal) {
        self.positive = positive;
    }

    pub fn set_negative(&mut self, negative: BigDecimal) {
        self.negative = negative;
    }
}