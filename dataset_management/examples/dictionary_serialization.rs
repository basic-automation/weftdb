use anyhow::Result;
use dataset_management::types::{
    Dictionary, DictionaryConstraints, Steps, VariablilityType, Variability,
    Pattern, PatternID, Relative, MeasurementVector,
};
use bigdecimal::BigDecimal;
use splimes::Spline;
use std::str::FromStr;

#[tokio::main]
async fn main() -> Result<()> {
    // Create a dictionary with constraints
    let constraints = DictionaryConstraints {
        steps: Some(Steps {
            count: 10,
            interpolation: Spline::Cubic,
        }),
        variabilities: Some(vec![
            VariablilityType::MaximumStatic(Variability {
                value: BigDecimal::from_str("0.1")?,
            }),
            VariablilityType::AverageStatic(Variability {
                value: BigDecimal::from_str("0.05")?,
            }),
        ]),
    };

    let mut dictionary = Dictionary::new(
        "Market Patterns".to_string(),
        "Common price movement patterns in financial markets".to_string(),
        constraints,
    );

    // Add some example patterns
    let pattern1 = create_example_pattern("Bull Run", vec![1.0, 1.2, 1.5, 1.8, 2.0]);
    let pattern2 = create_example_pattern("Bear Market", vec![2.0, 1.8, 1.5, 1.2, 1.0]);
    let pattern3 = create_example_pattern("Sideways", vec![1.0, 1.1, 0.9, 1.05, 1.0]);

    dictionary.import_pattern(pattern1).await?;
    dictionary.import_pattern(pattern2).await?;
    dictionary.import_pattern(pattern3).await?;

    println!("Dictionary created with {} patterns", dictionary.len());

    // Demonstrate JSON serialization
    println!("\n=== JSON Serialization ===");
    let json = dictionary.to_json()?;
    println!("JSON size: {} bytes", json.len());
    
    // Save JSON to file
    std::fs::write("dictionary.json", &json)?;
    println!("Saved to dictionary.json");

    // Demonstrate pretty JSON serialization
    let pretty_json = dictionary.to_json_pretty()?;
    std::fs::write("dictionary_pretty.json", &pretty_json)?;
    println!("Saved pretty JSON to dictionary_pretty.json");

    // Demonstrate binary serialization
    println!("\n=== Binary Serialization ===");
    let bytes = dictionary.to_bytes()?;
    println!("Binary size: {} bytes", bytes.len());
    println!("Compression ratio: {:.2}%", (bytes.len() as f64 / json.len() as f64) * 100.0);
    
    // Save binary to file
    std::fs::write("dictionary.bin", &bytes)?;
    println!("Saved to dictionary.bin");

    // Demonstrate deserialization
    println!("\n=== Deserialization Test ===");
    
    // Load from JSON
    let loaded_json = std::fs::read_to_string("dictionary.json")?;
    let dict_from_json = Dictionary::from_json(&loaded_json)?;
    println!("Loaded from JSON: {} patterns", dict_from_json.len());
    assert_eq!(dictionary.id, dict_from_json.id);

    // Load from binary
    let loaded_bytes = std::fs::read("dictionary.bin")?;
    let dict_from_binary = Dictionary::from_bytes(&loaded_bytes)?;
    println!("Loaded from binary: {} patterns", dict_from_binary.len());
    assert_eq!(dictionary.id, dict_from_binary.id);

    // Show dictionary structure
    println!("\n=== Dictionary Structure ===");
    println!("ID: {}", dictionary.id);
    println!("Name: {}", dictionary.name);
    println!("Description: {}", dictionary.description);
    println!("Patterns: {}", dictionary.len());
    
    if let Some(steps) = &dictionary.constraints.steps {
        println!("Steps: {} with {:?} interpolation", steps.count, steps.interpolation);
    }
    
    if let Some(variabilities) = &dictionary.constraints.variabilities {
        println!("Variabilities: {} constraints", variabilities.len());
        for (i, var) in variabilities.iter().enumerate() {
            match var {
                VariablilityType::MaximumStatic(v) => {
                    println!("  {}: Maximum Static = {}", i + 1, v.value);
                }
                VariablilityType::AverageStatic(v) => {
                    println!("  {}: Average Static = {}", i + 1, v.value);
                }
                _ => {
                    println!("  {}: Other variability type", i + 1);
                }
            }
        }
    }

    println!("\n✅ Serialization demo completed successfully!");
    println!("📁 Files created:");
    println!("  - dictionary.json ({} bytes)", json.len());
    println!("  - dictionary_pretty.json ({} bytes)", pretty_json.len());
    println!("  - dictionary.bin ({} bytes)", bytes.len());

    Ok(())
}

fn create_example_pattern(_name: &str, values: Vec<f64>) -> Pattern {
    use bigdecimal::BigDecimal;

    let relatives: Vec<Relative> = values
        .iter()
        .enumerate()
        .map(|(i, &v)| {
            let location = i as f64 / (values.len() - 1) as f64;
            let vector = MeasurementVector::new(
                BigDecimal::from_str(&location.to_string()).unwrap(),
                BigDecimal::from_str(&v.to_string()).unwrap(),
            );
            Relative::new(
                vector,
                BigDecimal::from_str(&((values.len() - 1) as f64).to_string()).unwrap(),
                BigDecimal::from_str(&values.iter().cloned().fold(0.0f64, f64::max).to_string()).unwrap(),
            )
        })
        .collect();

    Pattern::new(
        PatternID::new(),
        vec![], // occurrences
        relatives,
    )
}