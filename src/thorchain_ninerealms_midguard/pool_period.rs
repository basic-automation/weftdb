use core::fmt::{Display, Formatter};

pub enum PoolPeriod {
	Hour,
	Day,
	Week,
	Month,
	ThreeMonth,
	HundredDay,
	SixMonth,
	Year,
	All,
	None,
}

impl Display for PoolPeriod {
	fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
		match self {
			PoolPeriod::Hour => write!(f, "1h"),
			PoolPeriod::Day => write!(f, "24h"),
			PoolPeriod::Week => write!(f, "7d"),
			PoolPeriod::Month => write!(f, "30d"),
			PoolPeriod::ThreeMonth => write!(f, "90d"),
			PoolPeriod::HundredDay => write!(f, "100d"),
			PoolPeriod::SixMonth => write!(f, "180d"),
			PoolPeriod::Year => write!(f, "365d"),
			PoolPeriod::All => write!(f, "all"),
			PoolPeriod::None => write!(f, ""),
		}
	}
}
