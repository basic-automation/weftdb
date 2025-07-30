use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SubjectId(Uuid);

impl SubjectId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SubjectId {
    fn default() -> Self {
        Self::new()
    }
}