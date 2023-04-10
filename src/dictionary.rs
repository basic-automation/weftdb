use super::{Constraints, Pattern};
use dsm_log::Log;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dictionary {
	pub id: Uuid,
	pub name: String,
	pub constraints: Constraints,
	pub patterns: Option<HashMap<Uuid, Pattern>>,
	pub log: Log,
	pub is_log: bool,
}

impl Dictionary {
	pub fn new(name: &str, constraints: Constraints, is_log: bool) -> Self {
		let log_name = name.replace(':', "_");
		let mut log = Log::new(&log_name);
		if is_log {
			log.log("Created Dictionary", &json!(name));
		}
		Self { id: Uuid::new_v4(), name: name.to_string(), constraints, patterns: None, log, is_log }
	}

	pub async fn add_pattern(&mut self, mut pattern: Pattern) {
		let patterns = match self.patterns {
			Some(ref patterns) => patterns.clone(),
			None => HashMap::new(),
		};

		// log begin
		if self.is_log {
                        let mut location_amplitudes = HashMap::new();

                        for (lk, lv) in pattern.locations.iter() {
                                for(ak, av) in pattern.amplitudes.iter() {
                                        if lk == ak {
                                                location_amplitudes.insert(lv.to_string(), av.clone());
                                        }
                                }
                        }                        
			self.log.log(&format!("{}: before steps enforced", pattern.id), &json!(location_amplitudes));
		}
		// log end
		pattern = match self.constraints.steps.is_enforced {
			true => pattern.enforce_steps(self.constraints.steps.count as f64, self.constraints.steps.interpolation.clone()).await,
			false => pattern,
		};

		// log begin
		if self.is_log {
			let mut location_amplitudes = HashMap::new();

                        for (lk, lv) in pattern.locations.iter() {
                                for(ak, av) in pattern.amplitudes.iter() {
                                        if lk == ak {
                                                location_amplitudes.insert(lv.to_string(), av.clone());
                                        }
                                }
                        }
			self.log.log(&format!("{}: after steps enforced", pattern.id), &json!(location_amplitudes));
		}
		// log end

                
                // add pattern to dictionary
		let mut patterns = patterns.clone();
		patterns.insert(pattern.id, pattern.clone());
		self.patterns = Some(patterns);

		if let true = self.constraints.variability.static_variability.enforced {
                        // log begin
                        if self.is_log {
                                let log_patterns = self.patterns.clone().unwrap_or(HashMap::new());
                                let log_patterns: HashMap<String, Pattern> = log_patterns.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
                                self.log.log("Patterns before enforcing static variability", &json!(log_patterns));
                        }
                        // log end

			println!("Enforcing static variability");
			self.enforce_static_variability(pattern.clone()).await;

                        // log begin
                        if self.is_log {
                                let log_patterns = self.patterns.clone().unwrap_or(HashMap::new());
                                let log_patterns: HashMap<String, Pattern> = log_patterns.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
                                self.log.log("Patterns after enforcing static variability", &json!(log_patterns));
                        }
                        // log end
		}

		if let true = self.constraints.variability.absolute_static_variability.enforced {
                        // log begin
                        if self.is_log {
                                let log_patterns = self.patterns.clone().unwrap_or(HashMap::new());
                                let log_patterns: HashMap<String, Pattern> = log_patterns.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
                                self.log.log("Patterns before enforcing absolute static variability", &json!(log_patterns));
                        }
                        // log end

			println!("Enforcing absolute static variability");
			self.enforce_absolute_static_variability(pattern.clone()).await;

                        // log begin
                        if self.is_log {
                                let log_patterns = self.patterns.clone().unwrap_or(HashMap::new());
                                let log_patterns: HashMap<String, Pattern> = log_patterns.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
                                self.log.log("Patterns after enforcing absolute static variability", &json!(log_patterns));
                        }
                        // log end
		}

		if let true = self.constraints.variability.percentage_variability.enforced {
                        // log begin
                        if self.is_log {
                                let log_patterns = self.patterns.clone().unwrap_or(HashMap::new());
                                let log_patterns: HashMap<String, Pattern> = log_patterns.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
                                self.log.log("Patterns before enforcing percentage variability", &json!(log_patterns));
                        }
                        // log end

			println!("Enforcing percentage variability");
			self.enforce_percent_variability(pattern.clone()).await;

                        // log begin
                        if self.is_log {
                                let log_patterns = self.patterns.clone().unwrap_or(HashMap::new());
                                let log_patterns: HashMap<String, Pattern> = log_patterns.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
                                self.log.log("Patterns after enforcing percentage variability", &json!(log_patterns));
                        }
                        // log end
		}

		if let true = self.constraints.variability.absolute_percentage_variability.enforced {
                        // log begin
                        if self.is_log {
                                let log_patterns = self.patterns.clone().unwrap_or(HashMap::new());
                                let log_patterns: HashMap<String, Pattern> = log_patterns.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
                                self.log.log("Patterns before enforcing absolute percentage variability", &json!(log_patterns));
                        }
                        // log end

			println!("Enforcing absolute percentage variability");
			self.enforce_absolute_percent_variability(pattern).await;

                        // log begin
                        if self.is_log {
                                let log_patterns = self.patterns.clone().unwrap_or(HashMap::new());
                                let log_patterns: HashMap<String, Pattern> = log_patterns.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
                                self.log.log("Patterns after enforcing absolute percentage variability", &json!(log_patterns));
                        }
                        // log end
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

	async fn enforce_absolute_static_variability(&mut self, new_pattern: Pattern) -> u64 {
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

			let (_, existing_pattern) = patts.iter().nth(i).unwrap();
			let mut existing_pattern = existing_pattern.clone();

			if new_pattern.id == existing_pattern.id {
				i += 1;
				continue;
			}

			// check variability
			let variability = new_pattern.absolute_static_variability(&existing_pattern.clone(), &self.constraints.variability.percentage_variability.type_).await;
			if variability.abs() > self.constraints.variability.percentage_variability.value {
				i += 1;
				continue;
			}

			// check if the the patterns have occurrences with start times that are within the same self.constraints.occurrences.distance.value
			if let true = self.constraints.occurrences.distance.enforced {
				let mut same_occurrence = false;
				'outer: for new_occurrence in new_pattern.occurrences.iter() {
					for existing_occurence in existing_pattern.occurrences.iter() {
						if (new_occurrence.1.start.clone() - existing_occurence.1.start.clone()).abs() <= self.constraints.occurrences.distance.value || (new_occurrence.1.end.clone() - existing_occurence.1.end.clone()).abs() <= self.constraints.occurrences.distance.value {
							same_occurrence = true;
							break 'outer;
						}
					}
				}

				if same_occurrence {
                                        println!("pattern is represented...");
					//i = i.saturating_sub(1);
					represented_pattern = true;
					i += 1;
					continue;
				}
			}

			// if variability is less than the percentage variability, then merge the patterns
			//pattern.merge_occurrences(&compare_pattern.clone());
                        println!("merging patterns...");
			existing_pattern.merge_occurrences(&new_pattern.clone()).await;

			// replace the existing pattern in the dictionary
			self.patterns.as_mut().unwrap().insert(existing_pattern.id, existing_pattern.clone());

			// remove new pattern from the dictionary
			//self.patterns.as_mut().unwrap().remove(&new_pattern.id);
			i = i.saturating_sub(1);

			// increment the number of merged patterns
			number_of_merged_patterns += 1;

			// set matched to true
			matched = true;

			i += 1;
		}

		if !matched && !represented_pattern {
			self.patterns.as_mut().unwrap().insert(new_pattern.id, new_pattern.clone());
		} else {
                        self.patterns.as_mut().unwrap().remove(&new_pattern.id);
                }

		number_of_merged_patterns
	}

	async fn enforce_static_variability(&mut self, new_pattern: Pattern) -> u64 {
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

			let (_, existing_pattern) = patts.iter().nth(i).unwrap();
			let mut existing_pattern = existing_pattern.clone();

			if new_pattern.id == existing_pattern.id {
				i += 1;
				continue;
			}

			// check variability
			let variability = new_pattern.static_variability(&existing_pattern.clone(), &self.constraints.variability.percentage_variability.type_).await;
			if variability.abs() > self.constraints.variability.percentage_variability.value {
				i += 1;
				continue;
			}

			// check if the the patterns have occurrences with start times that are within the same self.constraints.occurrences.distance.value
			if let true = self.constraints.occurrences.distance.enforced {
				let mut same_occurrence = false;
				'outer: for new_occurrence in new_pattern.occurrences.iter() {
					for existing_occurence in existing_pattern.occurrences.iter() {
						if (new_occurrence.1.start.clone() - existing_occurence.1.start.clone()).abs() <= self.constraints.occurrences.distance.value || (new_occurrence.1.end.clone() - existing_occurence.1.end.clone()).abs() <= self.constraints.occurrences.distance.value {
							same_occurrence = true;
							break 'outer;
						}
					}
				}

				if same_occurrence {
                                        println!("pattern is represented...");
					//i = i.saturating_sub(1);
					represented_pattern = true;
					i += 1;
					continue;
				}
			}

			// if variability is less than the percentage variability, then merge the patterns
			//pattern.merge_occurrences(&compare_pattern.clone());
                        println!("merging patterns...");
			existing_pattern.merge_occurrences(&new_pattern.clone()).await;

			// replace the existing pattern in the dictionary
			self.patterns.as_mut().unwrap().insert(existing_pattern.id, existing_pattern.clone());

			// remove new pattern from the dictionary
			//self.patterns.as_mut().unwrap().remove(&new_pattern.id);
			i = i.saturating_sub(1);

			// increment the number of merged patterns
			number_of_merged_patterns += 1;

			// set matched to true
			matched = true;

			i += 1;
		}

		if !matched && !represented_pattern {
			self.patterns.as_mut().unwrap().insert(new_pattern.id, new_pattern.clone());
		} else {
                        self.patterns.as_mut().unwrap().remove(&new_pattern.id);
                }

		number_of_merged_patterns
	}

	async fn enforce_absolute_percent_variability(&mut self, new_pattern: Pattern) -> u64 {
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

			let (_, existing_pattern) = patts.iter().nth(i).unwrap();
			let mut existing_pattern = existing_pattern.clone();

			if new_pattern.id == existing_pattern.id {
				i += 1;
				continue;
			}

			// check variability
			let variability = new_pattern.absolute_percentage_variability(&existing_pattern.clone(), &self.constraints.variability.percentage_variability.type_).await;
			if variability.abs() > self.constraints.variability.percentage_variability.value {
				i += 1;
				continue;
			}

			// check if the the patterns have occurrences with start times that are within the same self.constraints.occurrences.distance.value
			if let true = self.constraints.occurrences.distance.enforced {
				let mut same_occurrence = false;
				'outer: for new_occurrence in new_pattern.occurrences.iter() {
					for existing_occurence in existing_pattern.occurrences.iter() {
						if (new_occurrence.1.start.clone() - existing_occurence.1.start.clone()).abs() <= self.constraints.occurrences.distance.value || (new_occurrence.1.end.clone() - existing_occurence.1.end.clone()).abs() <= self.constraints.occurrences.distance.value {
							same_occurrence = true;
							break 'outer;
						}
					}
				}

				if same_occurrence {
                                        println!("pattern is represented...");
					//i = i.saturating_sub(1);
					represented_pattern = true;
					i += 1;
					continue;
				}
			}

			// if variability is less than the percentage variability, then merge the patterns
			//pattern.merge_occurrences(&compare_pattern.clone());
                        println!("merging patterns...");
			existing_pattern.merge_occurrences(&new_pattern.clone()).await;

			// replace the existing pattern in the dictionary
			self.patterns.as_mut().unwrap().insert(existing_pattern.id, existing_pattern.clone());

			// remove new pattern from the dictionary
			//self.patterns.as_mut().unwrap().remove(&new_pattern.id);
			i = i.saturating_sub(1);

			// increment the number of merged patterns
			number_of_merged_patterns += 1;

			// set matched to true
			matched = true;

			i += 1;
		}

		if !matched && !represented_pattern {
			self.patterns.as_mut().unwrap().insert(new_pattern.id, new_pattern.clone());
		} else {
                        self.patterns.as_mut().unwrap().remove(&new_pattern.id);
                }

		number_of_merged_patterns
	}

	async fn enforce_percent_variability(&mut self, new_pattern: Pattern) -> u64 {
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

			let (_, existing_pattern) = patts.iter().nth(i).unwrap();
			let mut existing_pattern = existing_pattern.clone();

			if new_pattern.id == existing_pattern.id {
				i += 1;
				continue;
			}

			// check variability
			let variability = new_pattern.percentage_variability(&existing_pattern.clone(), &self.constraints.variability.percentage_variability.type_).await;
			if variability.abs() > self.constraints.variability.percentage_variability.value {
				i += 1;
				continue;
			}

			// check if the the patterns have occurrences with start times that are within the same self.constraints.occurrences.distance.value
			if let true = self.constraints.occurrences.distance.enforced {
				let mut same_occurrence = false;
				'outer: for new_occurrence in new_pattern.occurrences.iter() {
					for existing_occurence in existing_pattern.occurrences.iter() {
						if (new_occurrence.1.start.clone() - existing_occurence.1.start.clone()).abs() <= self.constraints.occurrences.distance.value || (new_occurrence.1.end.clone() - existing_occurence.1.end.clone()).abs() <= self.constraints.occurrences.distance.value {
							same_occurrence = true;
							break 'outer;
						}
					}
				}

				if same_occurrence {
                                        println!("pattern is represented...");
					//i = i.saturating_sub(1);
					represented_pattern = true;
					i += 1;
					continue;
				}
			}

			// if variability is less than the percentage variability, then merge the patterns
			//pattern.merge_occurrences(&compare_pattern.clone());
                        println!("merging patterns...");
			existing_pattern.merge_occurrences(&new_pattern.clone()).await;

			// replace the existing pattern in the dictionary
			self.patterns.as_mut().unwrap().insert(existing_pattern.id, existing_pattern.clone());

			// remove new pattern from the dictionary
			//self.patterns.as_mut().unwrap().remove(&new_pattern.id);
			i = i.saturating_sub(1);

			// increment the number of merged patterns
			number_of_merged_patterns += 1;

			// set matched to true
			matched = true;

			i += 1;
		}

		if !matched && !represented_pattern {
			self.patterns.as_mut().unwrap().insert(new_pattern.id, new_pattern.clone());
		} else {
                        self.patterns.as_mut().unwrap().remove(&new_pattern.id);
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

	pub async fn finish(&mut self) {
		if self.is_log {
			let _ = self.log.save("pattern").await;
		}
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
