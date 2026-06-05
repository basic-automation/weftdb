use super::Pattern;
use dsm_config::DatasetConfiguration;
use dsm_log::Log;
use rayon::iter::IntoParallelIterator;
use rayon::iter::IntoParallelRefIterator;
use rayon::iter::ParallelIterator;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::thread;
use std::time::Instant;
use threadpool::ThreadPool;
use tokio::sync::Mutex as TokMutex;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dictionary {
	pub id: Uuid,
	pub name: String,
	pub configuration: DictionaryConfiguration,
	pub patterns: DictionaryPatterns,
	pub log: Log,
	pub is_log: bool,
}

#[derive(Debug, Clone)]
pub struct DictionaryConfiguration(pub Arc<TokMutex<DatasetConfiguration>>);

impl Serialize for DictionaryConfiguration {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		let s = Arc::clone(&self.0);
		let t = thread::spawn(move || {
			let rt = tokio::runtime::Runtime::new().unwrap();
			let local_s = Arc::clone(&s);
			let p = rt.block_on(local_s.lock()).clone();
			drop(s);
			p
		});

		let config = t.join().unwrap();
		config.serialize(serializer)
	}
}

impl<'de> Deserialize<'de> for DictionaryConfiguration {
	fn deserialize<D>(deserializer: D) -> Result<Self, <D as serde::Deserializer<'de>>::Error>
	where
		D: serde::Deserializer<'de>,
	{
		let config = DatasetConfiguration::deserialize(deserializer)?;
		Ok(Self(Arc::new(TokMutex::new(config))))
	}
}

#[derive(Debug, Clone, Default)]
pub struct DictionaryPatterns(pub Option<Arc<TokMutex<HashMap<Uuid, Pattern>>>>);

impl Serialize for DictionaryPatterns {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		let patterns = match &self.0 {
			Some(patterns) => {
				let s = Arc::clone(patterns);
				let t = thread::spawn(move || {
					let rt = tokio::runtime::Runtime::new().unwrap();
					let p = rt.block_on(s.lock()).clone();
					drop(s);
					p
				});
				t.join().unwrap()
			}
			None => HashMap::new(),
		};
		let patterns: HashMap<String, Pattern> = patterns.par_iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
		patterns.serialize(serializer)
	}
}

impl<'de> Deserialize<'de> for DictionaryPatterns {
	fn deserialize<D>(deserializer: D) -> Result<Self, <D as serde::Deserializer<'de>>::Error>
	where
		D: serde::Deserializer<'de>,
	{
		let patterns: HashMap<String, Pattern> = HashMap::deserialize(deserializer)?;
		let patterns: HashMap<Uuid, Pattern> = patterns.par_iter().map(|(k, v)| (Uuid::parse_str(k).unwrap(), v.clone())).collect();
		Ok(Self(Some(Arc::new(TokMutex::new(patterns)))))
	}
}

impl Dictionary {
	pub fn new(name: &str, config: DatasetConfiguration, is_log: bool) -> Self {
		let log_name = name.replace(':', "_");
		let mut log = Log::new(&log_name);
		if is_log {
			log.log("Created Dictionary", &json!(name));
		}
		let config = DictionaryConfiguration(Arc::new(TokMutex::new(config)));
		Self { id: Uuid::new_v4(), name: name.to_string(), patterns: DictionaryPatterns::default(), log, is_log, configuration: config }
	}

	pub async fn add_pattern(&mut self, mut pattern: Pattern) {
		let patterns = match &self.patterns.0 {
			Some(patterns) => Arc::clone(patterns),
			None => {
				self.patterns.0 = Some(Arc::new(TokMutex::new(HashMap::new())));
				Arc::clone(self.patterns.0.as_ref().unwrap())
			}
		};
		let config = Arc::clone(&self.configuration.0);

		// log begin
		if self.is_log {
			let mut location_amplitudes = HashMap::new();

			for (lk, lv) in pattern.locations.iter() {
				for (ak, av) in pattern.amplitudes.iter() {
					if lk == ak {
						location_amplitudes.insert(lv.to_string(), av.clone());
					}
				}
			}
			self.log.log(&format!("{}: before steps enforced", pattern.id), &json!(location_amplitudes));
		}
		// log end
		let local_config = config.lock().await;
		pattern = match local_config.patterns.constraints.steps.is_enforced {
			true => {
				let take = local_config.patterns.constraints.steps.value.clone();
				let interpolation = local_config.patterns.constraints.steps.interpolation.clone();
				drop(local_config);
				pattern.enforce_steps(take, interpolation).await
			}
			false => {
				drop(local_config);
				pattern
			}
		};

		// log begin
		if self.is_log {
			let mut location_amplitudes = HashMap::new();

			for (lk, lv) in pattern.locations.iter() {
				for (ak, av) in pattern.amplitudes.iter() {
					if lk == ak {
						location_amplitudes.insert(lv.to_string(), av.clone());
					}
				}
			}
			self.log.log(&format!("{}: after steps enforced", pattern.id), &json!(location_amplitudes));
		}
		// log end

		// add pattern to dictionary
		let mut local_patterns = patterns.lock().await;
		local_patterns.insert(pattern.id, pattern.clone());
		drop(local_patterns);

		let local_config = config.lock().await;
		let static_variability_enforced = local_config.patterns.constraints.static_variability.is_enforced;
		let absolute_static_variability_enforced = local_config.patterns.constraints.absolute_static_variability.is_enforced;
		let percentage_variability_enforced = local_config.patterns.constraints.percentage_variability.is_enforced;
		let absolute_percentage_variability_enforced = local_config.patterns.constraints.absolute_percentage_variability.is_enforced;
		drop(local_config);
		if static_variability_enforced {
			// log begin
			if self.is_log {
				let local_patterns = patterns.lock().await;
				let log_patterns = local_patterns.clone();
				drop(local_patterns);
				let log_patterns: HashMap<String, Pattern> = log_patterns.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
				self.log.log("Patterns before enforcing static variability", &json!(log_patterns));
			}
			// log end

			//println!("Enforcing static variability");
			self.enforce_static_variability(pattern.clone()).await.unwrap();

			// log begin
			if self.is_log {
				let local_patterns = patterns.lock().await;
				let log_patterns = local_patterns.clone();
				drop(local_patterns);
				let log_patterns: HashMap<String, Pattern> = log_patterns.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
				self.log.log("Patterns after enforcing static variability", &json!(log_patterns));
			}
			// log end
		}

		if absolute_static_variability_enforced {
			// log begin
			if self.is_log {
				let local_patterns = patterns.lock().await;
				let log_patterns = local_patterns.clone();
				drop(local_patterns);
				let log_patterns: HashMap<String, Pattern> = log_patterns.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
				self.log.log("Patterns before enforcing absolute static variability", &json!(log_patterns));
			}
			// log end

			//println!("Enforcing absolute static variability");
			self.enforce_absolute_static_variability(pattern.clone()).await.unwrap();

			// log begin
			if self.is_log {
				let local_patterns = patterns.lock().await;
				let log_patterns = local_patterns.clone();
				drop(local_patterns);
				let log_patterns: HashMap<String, Pattern> = log_patterns.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
				self.log.log("Patterns after enforcing absolute static variability", &json!(log_patterns));
			}
			// log end
		}

		if percentage_variability_enforced {
			// log begin
			if self.is_log {
				let local_patterns = patterns.lock().await;
				let log_patterns = local_patterns.clone();
				drop(local_patterns);
				let log_patterns: HashMap<String, Pattern> = log_patterns.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
				self.log.log("Patterns before enforcing percentage variability", &json!(log_patterns));
			}
			// log end

			//println!("Enforcing percentage variability");
			self.enforce_percent_variability(pattern.clone()).await.unwrap();

			// log begin
			if self.is_log {
				let local_patterns = patterns.lock().await;
				let log_patterns = local_patterns.clone();
				drop(local_patterns);
				let log_patterns: HashMap<String, Pattern> = log_patterns.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
				self.log.log("Patterns after enforcing percentage variability", &json!(log_patterns));
			}
			// log end
		}

		if absolute_percentage_variability_enforced {
			// log begin
			if self.is_log {
				let local_patterns = patterns.lock().await;
				let log_patterns = local_patterns.clone();
				drop(local_patterns);
				let log_patterns: HashMap<String, Pattern> = log_patterns.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
				self.log.log("Patterns before enforcing absolute percentage variability", &json!(log_patterns));
			}
			// log end

			//println!("Enforcing absolute percentage variability");
			self.enforce_absolute_percent_variability(pattern).await.unwrap();

			// log begin
			if self.is_log {
				let local_patterns = patterns.lock().await;
				let log_patterns = local_patterns.clone();
				drop(local_patterns);
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
		let configuration = self.configuration.0.clone();
		let local_configureation = configuration.lock().await;
		let enforce_static_variability = local_configureation.patterns.constraints.static_variability.is_enforced;
		let enforce_absolute_static_variability = local_configureation.patterns.constraints.absolute_static_variability.is_enforced;
		let enforce_percent_variability = local_configureation.patterns.constraints.percentage_variability.is_enforced;
		let enforce_absolute_percent_variability = local_configureation.patterns.constraints.absolute_percentage_variability.is_enforced;
		drop(local_configureation);

		let patterns = match &self.patterns.0 {
			Some(p) => Arc::clone(p),
			None => {
				println!("Patterns are not set");
				return 0;
			}
		};

		let local_patterns = patterns.lock().await;
		let patterns = local_patterns.clone();
		drop(local_patterns);

		let mut number_of_merged_patterns: u64 = 0;

		if enforce_static_variability || enforce_absolute_static_variability || enforce_percent_variability || enforce_absolute_percent_variability {
			let mut i = 0;
			loop {
				if i >= patterns.len() {
					break;
				}

				let patts = patterns.clone();
				let (_, pattern) = patts.iter().nth(i).unwrap();

				if let true = enforce_static_variability {
					number_of_merged_patterns += self.enforce_static_variability(pattern.clone()).await.unwrap();
				}

				if let true = enforce_absolute_static_variability {
					number_of_merged_patterns += self.enforce_absolute_static_variability(pattern.clone()).await.unwrap();
				}

				if let true = enforce_percent_variability {
					number_of_merged_patterns += self.enforce_percent_variability(pattern.clone()).await.unwrap();
				}

				if let true = enforce_absolute_static_variability {
					number_of_merged_patterns += self.enforce_absolute_static_variability(pattern.clone()).await.unwrap();
				}

				if let true = enforce_absolute_percent_variability {
					number_of_merged_patterns += self.enforce_absolute_percent_variability(pattern.clone()).await.unwrap();
				}

				i += 1;
			}
		}

		number_of_merged_patterns
	}

	async fn enforce_absolute_static_variability(&mut self, _new_pattern: Pattern) -> Result<u64, String> {
		todo!()
	}

	async fn enforce_static_variability(&mut self, new_pattern: Pattern) -> Result<u64, String> {
		let new_pattern = Arc::new(TokMutex::new(new_pattern));
		let new_pattern_2 = Arc::clone(&new_pattern);
		let number_of_merged_patterns = Arc::new(TokMutex::new(0_u64));
		let number_of_merged_patterns_2 = Arc::clone(&number_of_merged_patterns);
		let matched = Arc::new(TokMutex::new(false));
		let matched_2 = Arc::clone(&matched);
		// represented_pattern is a pattern that has matched an existing occurrence
		let represented_pattern = Arc::new(TokMutex::new(false));
		let represented_pattern_2 = Arc::clone(&represented_pattern);
		let patterns = match self.patterns.0 {
			Some(ref patterns) => Arc::clone(patterns),
			None => return Err("No patterns found".to_string()),
		};
		let patterns_2 = Arc::clone(&patterns);
		let configuration = Arc::clone(&self.configuration.0);

		let pool = ThreadPool::with_name("enforce_static_variability".into(), 1);
		pool.execute(move || {
			let rt = tokio::runtime::Runtime::new().unwrap();
			let patterns = Arc::clone(&patterns);
			let local_patts = rt.block_on(patterns.lock());
			let local_patterns = local_patts.clone();
			drop(local_patts);
			let represented_pattern = Arc::clone(&represented_pattern);

			let local_patterns = local_patterns.into_par_iter().collect::<Vec<(_, Pattern)>>();

			local_patterns.par_iter().for_each(|(_, existing_pattern)| {
				let rt = tokio::runtime::Runtime::new().unwrap();
				let new_pattern = Arc::clone(&new_pattern);
				let mut existing_pattern = existing_pattern.clone();
				let represented_pattern = Arc::clone(&represented_pattern);

				// if the new pattern is the same as the existing pattern, then return
				let local_new_pattern = rt.block_on(new_pattern.lock());
				if local_new_pattern.id == existing_pattern.id {
					return;
				}
				drop(local_new_pattern);

				// get configuration
				let local_configuration = Arc::clone(&configuration);
				let local_configuration = rt.block_on(local_configuration.lock());
				let static_variability_type = local_configuration.patterns.constraints.static_variability.type_.clone();
				let static_variability_value = local_configuration.patterns.constraints.static_variability.value.clone();
				let occurrence_distance_is_enforced = local_configuration.patterns.constraints.occurrences.distance.is_enforced;
				let occurrences_distance_value = local_configuration.patterns.constraints.occurrences.distance.value.clone();
				drop(local_configuration);

				// check variability
				let local_new_pattern = rt.block_on(new_pattern.lock());
				let variability = rt.block_on(local_new_pattern.static_variability(&existing_pattern.clone(), &static_variability_type));
				drop(local_new_pattern);

				if variability.abs() > static_variability_value {
					return;
				}

				// check if the the patterns have occurrences with start times that are within the same self.constraints.occurrences.distance.value
				if occurrence_distance_is_enforced {
					let mut same_occurrence = false;
					let local_new_pattern = rt.block_on(new_pattern.lock());
					'outer: for new_occurrence in local_new_pattern.occurrences.iter() {
						for existing_occurence in existing_pattern.occurrences.iter() {
							if (new_occurrence.1.start.clone() - existing_occurence.1.start.clone()).abs() <= occurrences_distance_value || (new_occurrence.1.end.clone() - existing_occurence.1.end.clone()).abs() <= occurrences_distance_value {
								same_occurrence = true;
								break 'outer;
							}
						}
					}
					drop(local_new_pattern);

					if same_occurrence {
						let mut local_represented_pattern = rt.block_on(represented_pattern.lock());
						*local_represented_pattern = true;
						drop(local_represented_pattern);
						return;
					}
				}

				// if variability is less than the percentage variability, then merge the patterns
				//pattern.merge_occurrences(&compare_pattern.clone());
				//println!("merging patterns...");
				let local_new_pattern = rt.block_on(new_pattern.lock());
				rt.block_on(existing_pattern.merge_occurrences(&local_new_pattern.clone()));
				drop(local_new_pattern);

				// replace the existing pattern in the dictionary
				let mut local_patterns = rt.block_on(patterns.lock());
				local_patterns.insert(existing_pattern.id, existing_pattern.clone());
				drop(local_patterns);

				// increment the number of merged patterns
				let mut local_number_of_merged_patterns = rt.block_on(number_of_merged_patterns.lock());
				*local_number_of_merged_patterns += 1;
				drop(local_number_of_merged_patterns);

				// set matched to true
				let mut local_matched = rt.block_on(matched.lock());
				*local_matched = true;
				drop(local_matched);
			});
		});
		pool.join();

		let local_represented_pattern = represented_pattern_2.lock().await;
		let local_matched = matched_2.lock().await;
		if !*local_matched && !*local_represented_pattern {
			let mut local_patterns = patterns_2.lock().await;
			let local_new_pattern = new_pattern_2.lock().await;
			local_patterns.insert(local_new_pattern.id, local_new_pattern.clone());
			drop(local_patterns);
			drop(local_new_pattern);
		} else {
			let mut local_patterns = patterns_2.lock().await;
			let local_new_pattern = new_pattern_2.lock().await;
			local_patterns.remove(&local_new_pattern.id);
			drop(local_patterns);
			drop(local_new_pattern);
		}
		drop(local_represented_pattern);
		drop(local_matched);

		let local_number_of_merged_patterns = number_of_merged_patterns_2.lock().await;
		Ok(*local_number_of_merged_patterns)
	}

	async fn enforce_absolute_percent_variability(&mut self, _new_pattern: Pattern) -> Result<u64, String> {
		todo!()
	}

	async fn enforce_percent_variability(&mut self, new_pattern: Pattern) -> Result<u64, String> {
		let new_pattern = Arc::new(TokMutex::new(new_pattern));
		let new_pattern_2 = Arc::clone(&new_pattern);
		let number_of_merged_patterns = Arc::new(TokMutex::new(0_u64));
		let matched = Arc::new(TokMutex::new(false));
		// represented_pattern is a pattern that has matched an existing occurrence
		let represented_pattern = Arc::new(TokMutex::new(false));
		let patterns = match self.patterns.0 {
			Some(ref patterns) => Arc::clone(patterns),
			None => return Err("No patterns found".to_string()),
		};
		let patterns_2 = Arc::clone(&patterns);
		let configuration = Arc::clone(&self.configuration.0);

                {
                        let number_of_merged_patterns = Arc::clone(&number_of_merged_patterns);
                        let matched = Arc::clone(&matched);
                        let represented_pattern = Arc::clone(&represented_pattern);

        		let t = thread::spawn(move || {
        			let rt = tokio::runtime::Runtime::new().unwrap();
        			let patterns = Arc::clone(&patterns);
        			let local_patts = rt.block_on(patterns.lock());
        			let local_patterns = local_patts.clone();
        			drop(local_patts);
        			let represented_pattern = Arc::clone(&represented_pattern);
                                let number_of_merged_patterns = Arc::clone(&number_of_merged_patterns);
        			let local_patterns = local_patterns.into_par_iter().collect::<Vec<(_, Pattern)>>();
                                let matched = Arc::clone(&matched);
                        
        			local_patterns.par_iter().for_each(|(_, existing_pattern)| {
        				let rt = tokio::runtime::Runtime::new().unwrap();
        				let new_pattern = Arc::clone(&new_pattern);
        				let mut existing_pattern = existing_pattern.clone();
        				let represented_pattern = Arc::clone(&represented_pattern);
                                        let number_of_merged_patterns = Arc::clone(&number_of_merged_patterns);
                                        let matched = Arc::clone(&matched);

        				// if the new pattern is the same as the existing pattern, then return
        				let local_new_pattern = rt.block_on(new_pattern.lock());
        				if local_new_pattern.id == existing_pattern.id {
        					return;
        				}
        				drop(local_new_pattern);

        				// get configuration
        				let local_configuration = Arc::clone(&configuration);
        				let local_configuration = rt.block_on(local_configuration.lock());
        				let percentage_variability_type = local_configuration.patterns.constraints.percentage_variability.type_.clone();
        				let percentage_variability_value = local_configuration.patterns.constraints.percentage_variability.value.clone();
        				let occurrence_distance_is_enforced = local_configuration.patterns.constraints.occurrences.distance.is_enforced;
        				let occurrences_distance_value = local_configuration.patterns.constraints.occurrences.distance.value.clone();
        				drop(local_configuration);

        				// check variability
        				let local_new_pattern = rt.block_on(new_pattern.lock());
        				let variability = rt.block_on(local_new_pattern.percentage_variability(&existing_pattern.clone(), &percentage_variability_type));
        				drop(local_new_pattern);

        				//println!("variability: {}, percentage target: {}", variability.abs(), percentage_variability_value);
        				if variability.abs() > percentage_variability_value {
        					return;
        				}

        				// check if the the patterns have occurrences with start times that are within the same self.constraints.occurrences.distance.value
        				if occurrence_distance_is_enforced {
        					let mut same_occurrence = false;
        					let local_new_pattern = rt.block_on(new_pattern.lock());
        					'outer: for new_occurrence in local_new_pattern.occurrences.iter() {
        						for existing_occurence in existing_pattern.occurrences.iter() {
        							if (new_occurrence.1.start.clone() - existing_occurence.1.start.clone()).abs() <= occurrences_distance_value || (new_occurrence.1.end.clone() - existing_occurence.1.end.clone()).abs() <= occurrences_distance_value {
        								same_occurrence = true;
        								break 'outer;
        							}
        						}
        					}
        					drop(local_new_pattern);

        					if same_occurrence {
        						let mut local_represented_pattern = rt.block_on(represented_pattern.lock());
        						*local_represented_pattern = true;
        						drop(local_represented_pattern);
        						return;
        					}
        				}

        				// if variability is less than the percentage variability, then merge the patterns
        				//pattern.merge_occurrences(&compare_pattern.clone());
        				//println!("merging patterns...");
        				let local_new_pattern = rt.block_on(new_pattern.lock());
        				rt.block_on(existing_pattern.merge_occurrences(&local_new_pattern.clone()));
        				drop(local_new_pattern);

        				// replace the existing pattern in the dictionary
        				let mut local_patterns = rt.block_on(patterns.lock());
        				local_patterns.insert(existing_pattern.id, existing_pattern.clone());
        				drop(local_patterns);

        				// increment the number of merged patterns
        				let mut local_number_of_merged_patterns = rt.block_on(number_of_merged_patterns.lock());
        				*local_number_of_merged_patterns += 1;
        				drop(local_number_of_merged_patterns);

        				// set matched to true
        				let mut local_matched = rt.block_on(matched.lock());
        				*local_matched = true;
        				drop(local_matched);
        			});
        		});
                        t.join().unwrap();
                }
		
		let local_represented_pattern = represented_pattern.lock().await;
		let local_matched = matched.lock().await;
		if !*local_matched && !*local_represented_pattern {
			let mut local_patterns = patterns_2.lock().await;
			let local_new_pattern = new_pattern_2.lock().await;
			local_patterns.insert(local_new_pattern.id, local_new_pattern.clone());
			drop(local_patterns);
			drop(local_new_pattern);
		} else {
			let mut local_patterns = patterns_2.lock().await;
			let local_new_pattern = new_pattern_2.lock().await;
			local_patterns.remove(&local_new_pattern.id);
			drop(local_patterns);
			drop(local_new_pattern);
		}
		drop(local_represented_pattern);
		drop(local_matched);

		let local_number_of_merged_patterns = number_of_merged_patterns.lock().await.clone();
		//println!("number of merged patterns: {}", local_number_of_merged_patterns);
		Ok(local_number_of_merged_patterns)
	}

	// todo: implement in patterner
	pub async fn add_patterns(&mut self, patterns: Vec<Pattern>) {
		for (i, pattern) in patterns.clone().iter_mut().enumerate() {
			// start timer
			let start = Instant::now();

			self.add_pattern(pattern.clone()).await;

			// end timer
			let duration = start.elapsed();
			println!("{}: Added pattern: {} in {:?}ms", self.name, i, duration.as_millis());
		}
	}

	pub fn get_pattern(&self, id: Uuid) -> Option<Pattern> {
		let rt = tokio::runtime::Runtime::new().unwrap();
		let patterns = match self.patterns.0.as_ref() {
			Some(patterns) => Arc::clone(patterns),
			None => return None,
		};
		let local_patterns = rt.block_on(patterns.lock());
		local_patterns.get(&id).cloned()
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
