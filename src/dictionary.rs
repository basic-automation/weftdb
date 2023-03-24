use super::{Constraints, Pattern};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dictionary {
	pub id: Uuid,
	pub name: String,
	pub constraints: Constraints,
	pub patterns: Option<HashMap<Uuid, Pattern>>,
}

impl Dictionary {
	pub fn new(name: &str, constraints: Constraints) -> Self {
		Self { id: Uuid::new_v4(), name: name.to_string(), constraints, patterns: None }
	}

	pub async fn add_pattern(&mut self, mut pattern: Pattern) {
		let patterns = match self.patterns {
			Some(ref patterns) => patterns.clone(),
			None => HashMap::new(),
		};
		pattern = match self.constraints.steps.is_enforced {
			true => pattern.enforce_steps(1.0_f64 / self.constraints.steps.count as f64, self.constraints.steps.interpolation.clone()).await,
			false => pattern,
		};

		let mut patterns = patterns.clone();
		patterns.insert(pattern.id, pattern.clone());
		self.patterns = Some(patterns);

		if let true = self.constraints.variability.static_variability.enforced {
                        println!("Enforcing static variability");
			self.enforce_static_variability(pattern.clone()).await;
		}

		if let true = self.constraints.variability.absolute_static_variability.enforced {
                        println!("Enforcing absolute static variability");
			self.enforce_absolute_static_variability(pattern.clone()).await;
		}

		if let true = self.constraints.variability.percentage_variability.enforced {
                        println!("Enforcing percentage variability");
			self.enforce_percent_variability(pattern.clone()).await;
		}

		if let true = self.constraints.variability.absolute_percentage_variability.enforced {
                        println!("Enforcing absolute percentage variability");
			self.enforce_absolute_percent_variability(pattern).await;
		}
	}

	pub async fn enforce_variability(&mut self) {
		let mut num = 1_u64;
		while num > 0_u64 {
			num = self.enforce_variability_run_all().await;
			println!("Number of merged patterns: {}", num);
		}
	}

	pub async fn enforce_variability_run_all(&mut self) -> u64 {
		let enforce_static_variability = self.constraints.variability.static_variability.enforced;
		let enforce_absolute_static_variability = self.constraints.variability.absolute_static_variability.enforced;
		let enforce_percent_variability = self.constraints.variability.percentage_variability.enforced;
		let mut number_of_merged_patterns: u64 = 0;

		if let true = (enforce_percent_variability || enforce_static_variability) {
			let mut i = 0;
			loop {
				if i >= self.patterns.as_ref().unwrap().len() {
					break;
				}

				let patts = self.patterns.as_ref().unwrap().clone();
				let (_, pattern) = patts.iter().nth(i).unwrap();

				if let true = enforce_static_variability {
					number_of_merged_patterns += self.enforce_static_variability(pattern.clone()).await;
				}

				if let true = enforce_absolute_static_variability {
					number_of_merged_patterns += self.enforce_absolute_static_variability(pattern.clone()).await;
				}

				if let true = enforce_percent_variability {
					number_of_merged_patterns += self.enforce_percent_variability(pattern.clone()).await;
				}

				if let true = enforce_absolute_static_variability {
					number_of_merged_patterns += self.enforce_absolute_static_variability(pattern.clone()).await;
				}

				i += 1;
			}
		}

		number_of_merged_patterns
	}

	async fn enforce_absolute_static_variability(&mut self, pattern: Pattern) -> u64 {
		let mut number_of_merged_patterns: u64 = 0;
		let mut matched = false;
		// represented_pattern is a pattern that has matched an existing occurrence
		let mut represented_pattern = false;
		let mut i = 0;

		loop {
			if i >= self.patterns.as_ref().unwrap().len() {
				break;
			}

			let patts = self.patterns.as_ref().unwrap().clone();

			let (_, compare_pattern) = patts.iter().nth(i).unwrap();
			let mut compare_pattern = compare_pattern.clone();

			// check if the pattern is the same
			if pattern.id == compare_pattern.id {
				i += 1;
				continue;
			}

			// check variability
			let variability = pattern.absolute_static_variability(&compare_pattern.clone(), &self.constraints.variability.absolute_static_variability.type_).await;
			//println!("Variability: {}, Target: {}", variability, self.constraints.variability.static_variability.value.clone());
			if variability.abs() > self.constraints.variability.absolute_static_variability.value {
				i += 1;
				continue;
			}

			// check if the the patterns have occurrences with start times that are within the same self.constraints.occurrences.distance.value
			if let true = self.constraints.occurrences.distance.enforced {
				let mut same_occurrence = false;
				'outer: for occurrence in pattern.occurrences.iter() {
					for compare_occurrence in compare_pattern.occurrences.iter() {
						if (occurrence.1.start.clone() - compare_occurrence.1.start.clone()).abs() <= self.constraints.occurrences.distance.value || (occurrence.1.end.clone() - compare_occurrence.1.end.clone()).abs() <= self.constraints.occurrences.distance.value {
							same_occurrence = true;
							break 'outer;
						}
					}
				}

				if same_occurrence {
					// remove compare pattern from the dictionary
					self.patterns.as_mut().unwrap().remove(&compare_pattern.id);

					i = i.saturating_sub(1);

					represented_pattern = true;
					i += 1;
					continue;
				}
			}

			// if variability is less than the percentage variability, then merge the patterns
			//pattern.merge_occurrences(&compare_pattern.clone());
			compare_pattern.merge_occurrences(&pattern.clone());

			// replace the existing pattern in the dictionary
			self.patterns.as_mut().unwrap().insert(compare_pattern.id, compare_pattern.clone());

			// remove pattern from the dictionary
			self.patterns.as_mut().unwrap().remove(&pattern.id);
			i = i.saturating_sub(1);

			// increment the number of merged patterns
			number_of_merged_patterns += 1;

			// set matched to true
			matched = true;

			i += 1;
		}

		if !matched && !represented_pattern {
			self.patterns.as_mut().unwrap().insert(pattern.id, pattern.clone());
		}

		number_of_merged_patterns
	}

	async fn enforce_static_variability(&mut self, pattern: Pattern) -> u64 {
		let mut number_of_merged_patterns: u64 = 0;
		let mut matched = false;
		// represented_pattern is a pattern that has matched an existing occurrence
		let mut represented_pattern = false;
		let mut i = 0;

		loop {
			if i >= self.patterns.as_ref().unwrap().len() {
				break;
			}

			let patts = self.patterns.as_ref().unwrap().clone();

			let (_, compare_pattern) = patts.iter().nth(i).unwrap();
			let mut compare_pattern = compare_pattern.clone();

			if pattern.id == compare_pattern.id {
				i += 1;
				continue;
			}

			// check variability
			let variability = pattern.static_variability(&compare_pattern.clone(), &self.constraints.variability.static_variability.type_).await;
			//println!("Variability: {}, Target: {}", variability, self.constraints.variability.static_variability.value.clone());
			if variability.abs() > self.constraints.variability.static_variability.value {
				i += 1;
				continue;
			}

			// check if the the patterns have occurrences with start times that are within the same self.constraints.occurrences.distance.value
			if let true = self.constraints.occurrences.distance.enforced {
				let mut same_occurrence = false;
				'outer: for occurrence in pattern.occurrences.iter() {
					for compare_occurrence in compare_pattern.occurrences.iter() {
						if (occurrence.1.start.clone() - compare_occurrence.1.start.clone()).abs() <= self.constraints.occurrences.distance.value || (occurrence.1.end.clone() - compare_occurrence.1.end.clone()).abs() <= self.constraints.occurrences.distance.value {
							same_occurrence = true;
							break 'outer;
						}
					}
				}

				if same_occurrence {
					// remove compare pattern from the dictionary
					self.patterns.as_mut().unwrap().remove(&compare_pattern.id);
					i = i.saturating_sub(1);
					represented_pattern = true;
					i += 1;
					continue;
				}
			}

			// if variability is less than the percentage variability, then merge the patterns
			compare_pattern.merge_occurrences(&pattern.clone());
			//pattern.merge_occurrences(&compare_pattern.clone());

			// replace the existing pattern in the dictionary
			self.patterns.as_mut().unwrap().insert(compare_pattern.id, compare_pattern.clone());

			// remove pattern from the dictionary
			self.patterns.as_mut().unwrap().remove(&pattern.id);
			i = i.saturating_sub(1);

			// increment the number of merged patterns
			number_of_merged_patterns += 1;

			// set matched to true
			matched = true;

			i += 1;
		}

		if !matched && !represented_pattern {
			self.patterns.as_mut().unwrap().insert(pattern.id, pattern.clone());
		}

		number_of_merged_patterns
	}

	async fn enforce_absolute_percent_variability(&mut self, pattern: Pattern) -> u64 {
		let mut number_of_merged_patterns: u64 = 0;
		let mut matched = false;
		// represented_pattern is a pattern that has matched an existing occurrence
		let mut represented_pattern = false;
		let mut i = 0;

		loop {
			if i >= self.patterns.as_ref().unwrap().len() {
				break;
			}

			let patts = self.patterns.as_ref().unwrap().clone();

			let (_, compare_pattern) = patts.iter().nth(i).unwrap();
			let mut compare_pattern = compare_pattern.clone();

			if pattern.id == compare_pattern.id {
				i += 1;
				continue;
			}

			// check variability
			let variability = pattern.absolute_percentage_variability(&compare_pattern.clone(), &self.constraints.variability.absolute_percentage_variability.type_).await;
			//println!("Variability: {}, Target: {}", variability, &self.constraints.variability.absolute_percentage_variability.value.clone());
			if variability.abs() > self.constraints.variability.absolute_percentage_variability.value {
				i += 1;
				continue;
			}

			// check if the the patterns have occurrences with start times that are within the same self.constraints.occurrences.distance.value
			if let true = self.constraints.occurrences.distance.enforced {
				let mut same_occurrence = false;
				'outer: for occurrence in pattern.occurrences.iter() {
					for compare_occurrence in compare_pattern.occurrences.iter() {
						if (occurrence.1.start.clone() - compare_occurrence.1.start.clone()).abs() <= self.constraints.occurrences.distance.value || (occurrence.1.end.clone() - compare_occurrence.1.end.clone()).abs() <= self.constraints.occurrences.distance.value {
							same_occurrence = true;
							break 'outer;
						}
					}
				}

				if same_occurrence {
					// remove compare pattern from the dictionary
					self.patterns.as_mut().unwrap().remove(&compare_pattern.id);
					i = i.saturating_sub(1);
					represented_pattern = true;
					i += 1;
					continue;
				}
			}

			// if variability is less than the percentage variability, then merge the patterns
			//pattern.merge_occurrences(&compare_pattern.clone());
			compare_pattern.merge_occurrences(&pattern.clone());

			// replace the existing pattern in the dictionary
			self.patterns.as_mut().unwrap().insert(compare_pattern.id, compare_pattern.clone());

			// remove compare pattern from the dictionary
			self.patterns.as_mut().unwrap().remove(&pattern.id);
			i = i.saturating_sub(1);

			// increment the number of merged patterns
			number_of_merged_patterns += 1;

			// set matched to true
			matched = true;

			i += 1;
		}

		if !matched && !represented_pattern {
			self.patterns.as_mut().unwrap().insert(pattern.id, pattern.clone());
		}

		number_of_merged_patterns
	}

	async fn enforce_percent_variability(&mut self, pattern: Pattern) -> u64 {
		let mut number_of_merged_patterns: u64 = 0;
		let mut matched = false;
		// represented_pattern is a pattern that has matched an existing occurrence
		let mut represented_pattern = false;
		let mut i = 0;

		loop {
			if i >= self.patterns.as_ref().unwrap().len() {
				break;
			}

			let patts = self.patterns.as_ref().unwrap().clone();

			let (_, compare_pattern) = patts.iter().nth(i).unwrap();
			let mut compare_pattern = compare_pattern.clone();

			if pattern.id == compare_pattern.id {
				i += 1;
				continue;
			}

			// check variability
			let variability = pattern.percentage_variability(&compare_pattern.clone(), &self.constraints.variability.percentage_variability.type_).await;
			if variability.abs() > self.constraints.variability.percentage_variability.value {
				i += 1;
				continue;
			}

			// check if the the patterns have occurrences with start times that are within the same self.constraints.occurrences.distance.value
			if let true = self.constraints.occurrences.distance.enforced {
				let mut same_occurrence = false;
				'outer: for occurrence in pattern.occurrences.iter() {
					for compare_occurrence in compare_pattern.occurrences.iter() {
						if (occurrence.1.start.clone() - compare_occurrence.1.start.clone()).abs() <= self.constraints.occurrences.distance.value || (occurrence.1.end.clone() - compare_occurrence.1.end.clone()).abs() <= self.constraints.occurrences.distance.value {
							same_occurrence = true;
							break 'outer;
						}
					}
				}

				if same_occurrence {
					// remove compare pattern from the dictionary
					self.patterns.as_mut().unwrap().remove(&compare_pattern.id);
					i = i.saturating_sub(1);
					represented_pattern = true;
					i += 1;
					continue;
				}
			}

			// if variability is less than the percentage variability, then merge the patterns
			//pattern.merge_occurrences(&compare_pattern.clone());
			compare_pattern.merge_occurrences(&pattern.clone());

			// replace the existing pattern in the dictionary
			self.patterns.as_mut().unwrap().insert(compare_pattern.id, compare_pattern.clone());

			// remove compar pattern from the dictionary
			self.patterns.as_mut().unwrap().remove(&pattern.id);
			i = i.saturating_sub(1);

			// increment the number of merged patterns
			number_of_merged_patterns += 1;

			// set matched to true
			matched = true;

			i += 1;
		}

		if !matched && !represented_pattern {
			self.patterns.as_mut().unwrap().insert(pattern.id, pattern.clone());
		}

		number_of_merged_patterns
	}

	pub async fn add_patterns(&mut self, patterns: Vec<Pattern>) {
		for (_, pattern) in patterns.clone().iter_mut().enumerate() {
                        println!("Adding pattern: {}", pattern.id);
			self.add_pattern(pattern.clone()).await;
		}
	}

	pub fn get_pattern(&self, id: Uuid) -> Option<Pattern> {
		let patterns = self.patterns.as_ref().unwrap();
		patterns.get(&id).cloned()
	}

	pub fn get_pattern_mut(&mut self, id: Uuid) -> Option<&mut Pattern> {
		let patterns = self.patterns.as_mut().unwrap();
		for pattern in patterns.iter_mut() {
			if pattern.1.id == id {
				return Some(pattern.1);
			}
		}
		None
	}

	pub fn get_merged_pattern(&self, id: Uuid) -> Option<Pattern> {
		let patterns = self.patterns.as_ref().unwrap();
		for pattern in patterns.iter() {
			let merged_patterns = match pattern.1.merged_patterns {
				Some(ref merged_patterns) => merged_patterns.clone(),
				None => vec![],
			};
			for merged_pattern in merged_patterns.iter() {
				if merged_pattern == &id {
					return Some(pattern.1.clone());
				}
			}
		}
		None
	}
}

impl PartialEq for Dictionary {
	fn eq(&self, other: &Self) -> bool {
		self.name == other.name
	}
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatchedPatterns {
	pub patterns: Option<Vec<Pattern>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DictionaryCollection(pub Vec<Dictionary>);

impl DictionaryCollection {
	pub fn new() -> Self {
		DictionaryCollection(vec![])
	}

	pub fn get(&self, name: &str) -> Option<Dictionary> {
		for dictionary in self.0.clone().iter() {
			if dictionary.name == name {
				return Some(dictionary.clone());
			}
		}
		None
	}
}

impl Default for DictionaryCollection {
	fn default() -> Self {
		Self::new()
	}
}
