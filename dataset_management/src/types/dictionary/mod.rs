use uuid::Uuid;

pub struct Dictionary {
        id: Uuid,
        name: String,
        description: String,
        patterns: Vec<Pattern>,
        constraints: DictionaryConstraints,
}