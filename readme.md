## why are you doing this

LLMs between lack of GPU and relatively long running processes at home trying to run stuff offline is a pain so I wanted to make something to MITM chatgpt/ollama to act as a proxy for my projects to manage the long running requests, file storage, caching and queueing up requests to not make my little ollama instace barf on me if my wife and I both ask it for stuff.

Integration with opensearch so I can index this stuff and effectively use it in workflows easily 

Minio to store the requests and responses into a file system for long term storage/history and being able to manage documents and files for using in workflows

UI to see the requests and the queued requests and a place to start adding some reporting and statistics on API usage 

## goals

transparent to all the existing LLM providers APIs - i want to swap between chatGPTs API and purely offline ollama models without a nightmare. 

multiserver and weight balancing

find available server and queue when not available

tokens vs max concurrent

### end game?

template to define complex prompts and workflows using this with external data sources defined and schedule them on a cron (or trigger) to run complex workflows using my little local Ollama instance running deepseek/llama/etc

#### REQUIREMENT

Rust
Redis
Opensearch
Minio

ollama server
 - any model 
 - embedding model of some sort
    - ollama pull mxbai-embed-large 