#!/bin/bash

# Start MinIO server
docker run -d \
    --name minio1 \
    -p 9000:9000 \
    -p 9001:9001 \
    -e "MINIO_ROOT_USER=minioadmin" \
    -e "MINIO_ROOT_PASSWORD=minioadmin" \
    minio/minio server /data --console-address ":9001"

# Wait for MinIO to start
sleep 5

# Configure AWS CLI for MinIO
export AWS_ACCESS_KEY_ID=minioadmin
export AWS_SECRET_ACCESS_KEY=minioadmin
export AWS_ENDPOINT_URL=http://localhost:9000

# Create test buckets
aws --endpoint-url http://localhost:9000 s3 mb s3://bucket1
aws --endpoint-url http://localhost:9000 s3 mb s3://bucket2
aws --endpoint-url http://localhost:9000 s3 mb s3://bucket3
aws --endpoint-url http://localhost:9000 s3 mb s3://bucket4

# Create test file
echo "test content" > test.txt

# Test admin access
echo "Testing admin access..."
curl -v -H "x-api-key: admin-secret-key" http://localhost:8080/bucket1/test.txt -X PUT -d @test.txt
curl -v -H "x-api-key: admin-secret-key" http://localhost:8080/bucket1/test.txt
curl -v -H "x-api-key: admin-secret-key" http://localhost:8080/bucket1

# Test user1 access
echo "Testing user1 access..."
curl -v -H "x-api-key: user1-secret-key" http://localhost:8080/bucket1/test.txt -X PUT -d @test.txt
curl -v -H "x-api-key: user1-secret-key" http://localhost:8080/bucket1/test.txt
curl -v -H "x-api-key: user1-secret-key" http://localhost:8080/bucket2/test.txt -X PUT -d @test.txt || echo "Expected error: no access to bucket2"

# Test readonly access
echo "Testing readonly access..."
curl -v -H "x-api-key: readonly-secret-key" http://localhost:8080/bucket1/test.txt
curl -v -H "x-api-key: readonly-secret-key" http://localhost:8080/bucket1
curl -v -H "x-api-key: readonly-secret-key" http://localhost:8080/bucket1/test.txt -X PUT -d @test.txt || echo "Expected error: readonly user cannot write"

# Test rate limiting
echo "Testing rate limiting..."
for i in {1..101}; do
    curl -s -H "x-api-key: user1-secret-key" http://localhost:8080/bucket1/test.txt > /dev/null
    if [ $i -eq 101 ]; then
        echo "Rate limit should be hit now"
    fi
done

# Test invalid API key
echo "Testing invalid API key..."
curl -v -H "x-api-key: invalid-key" http://localhost:8080/bucket1/test.txt

# Cleanup
rm test.txt
docker stop minio1
docker rm minio1 