use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DatabaseId(Uuid);

impl DatabaseId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for DatabaseId {
    fn default() -> Self {
        Self::new()
    }
}
