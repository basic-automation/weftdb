use core::fmt::Display;

pub enum PoolStatus {
	Available,
	Staged,
	Suspended,
	None,
}

impl Display for PoolStatus {
	fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
		match self {
			PoolStatus::Available => write!(f, "available"),
			PoolStatus::Staged => write!(f, "staged"),
			PoolStatus::Suspended => write!(f, "suspended"),
			PoolStatus::None => write!(f, ""),
		}
	}
}
