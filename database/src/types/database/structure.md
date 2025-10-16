
# Database Structure

-> Self --- ./{db_name}
-> -> Subjects --- ./{subject_name}
-> -> -> Aspects --- ./{aspect_name}
-> -> -> -> Metadata --- ./metadata.db
-> -> -> -> Measurements --- ./measurements.db
-> -> -> -> -> Tables --- measurements
-> -> -> -> Batches --- ./batches.db
-> -> -> -> -> Tables --- unprocessed_batches | processed_batches
