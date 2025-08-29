## why are you doing this

Integration with opensearch so I can index this stuff and effectively use it in workflows easily 

Minio to store the requests and responses into a file system for long term storage/history and being able to manage documents and files for using in workflows

UI to see the requests and the queued requests and a place to start adding some reporting and statistics on API usage 

#### REQUIREMENT

Rust
Redis
Opensearch
Minio

ollama server
 - any model 
 - embedding model of some sort
    - ollama pull mxbai-embed-large 