
# Database Structure

-> Self --- ./{db_name}
-> -> Metadata --- ./metadata.db

-> -> Subjects --- ./{subject_name}
-> -> -> Aspects --- ./{aspect_name}
-> -> -> -> Measurements --- ./measurements.db
-> -> -> -> Unprocessed Batches --- ./unprocessed_batches.db
-> -> -> -> Processed Batches --- ./processed_batches.db
-> -> -> -> Patterns --- ./patterns.db
-> -> -> -> Events --- ./events.db
-> -> -> -> Correlations --- ./correlations.db

-> -> -> -> Dictionaries --- ./dictionaries
-> -> -> -> -> Dictionary --- ./{dictionary_name}.db