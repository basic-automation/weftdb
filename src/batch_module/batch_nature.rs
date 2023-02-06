use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchNature {
        pub max_x_movement: f64,
        pub max_y_movement: f64,
        pub relative_x_movements: Vec<f64>,
        pub relative_y_movements: Vec<f64>,
}