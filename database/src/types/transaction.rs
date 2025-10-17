use chrono::{DateTime, Utc};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TxId(Uuid);

impl TxId {
	#[must_use]
	pub fn new() -> Self {
		Self(Uuid::new_v4())
	}

	#[must_use]
	pub const fn from_uuid(uuid: Uuid) -> Self {
		Self(uuid)
	}

	#[must_use]
	pub const fn as_uuid(&self) -> Uuid {
		self.0
	}
}

impl Default for TxId {
	fn default() -> Self {
		Self::new()
	}
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Transaction {
	id: TxId,
	message: String,
	created_at: DateTime<Utc>,
}

impl Transaction {
	#[must_use] 
	pub fn new(id: Option<TxId>, message: String) -> Self {
		Self { id: id.unwrap_or_default(), message, created_at: chrono::Utc::now() }
	}

	#[must_use] 
	pub const fn id(&self) -> TxId {
		self.id
	}

	#[must_use] 
	pub fn message(&self) -> &str {
		&self.message
	}

	#[must_use] 
	pub const fn created_at(&self) -> DateTime<Utc> {
		self.created_at
	}
}
