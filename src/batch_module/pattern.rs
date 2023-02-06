use serde::{Deserialize, Serialize};
use crate::sources::Source;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pattern {
        pub uuid: String,
        pub size: usize,
        pub start_timestamp: i64,
        pub end_timestamp: i64,
        pub source: Source,
        pub x_coordinates: Vec<f64>,
        pub y_coordinates: Vec<f64>,
}

pub trait ToPattern {
        fn to_pattern(&self) -> Pattern;
}

/* 
        {
                "uuid": Uuid,
                "size": usize,
                "occurrence": {
                        "occurrence_source": Source,
                        "occurrence_uuid": Uuid,
                        "occurrence_start_timestamp": i64,
                        "occurrence_end_timestamp": i64,
                },
                "x_coordinates": Vec<f64>,
                "y_coordinates": Vec<f64>,
        }
*/