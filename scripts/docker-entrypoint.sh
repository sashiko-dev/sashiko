#!/bin/bash
set -e

KERNEL_DIR="${SASHIKO__GIT__REPOSITORY_PATH:-/app/third_party/linux}"
READY_FILE="$KERNEL_DIR/.ready"
# Use the bundle baked into the image
IMAGE_BUNDLE="/opt/linux-kernel.bundle"
BUNDLE_URL="https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/clone.bundle"

# Function to setup kernel in background
setup_kernel() {
    echo "Background task: Initializing/Updating Linux kernel working tree..."
    rm -f "$READY_FILE"

    if [ ! -d "$KERNEL_DIR/.git" ]; then
        echo "Initializing Linux kernel working tree..."
        
        if [ -f "$IMAGE_BUNDLE" ]; then
            echo "Using bundled kernel from image..."
            BUNDLE_PATH="$IMAGE_BUNDLE"
        else
            echo "Image bundle missing, downloading (~2.5GB)..."
            BUNDLE_PATH="/tmp/linux-kernel.bundle"
            wget -c "$BUNDLE_URL" -O "$BUNDLE_PATH"
        fi

        echo "Cloning from bundle..."
        mkdir -p "$KERNEL_DIR"
        git clone "$BUNDLE_PATH" "$KERNEL_DIR"
        
        cd "$KERNEL_DIR"
        echo "Configuring remotes..."
        git remote remove origin
        git remote add origin https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git
        
        echo "Pulling latest changes from master..."
        git pull origin master
        
        # Only cleanup if we downloaded it
        if [ "$BUNDLE_PATH" == "/tmp/linux-kernel.bundle" ]; then
            rm "$BUNDLE_PATH"
        fi
    else
        echo "Linux kernel tree already initialized. Updating and maintaining..."
        cd "$KERNEL_DIR"
        # Ensure remote origin is set correctly and not pointing to a temporary bundle
        git remote set-url origin https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git 2>/dev/null || \
        git remote add origin https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git
        # Prune unreachable objects and run gc to keep the repo healthy
        echo "Pruning and garbage collecting..."
        git remote prune origin || true
        git reflog expire --all --expire=now
        git gc --auto
        git prune
        # Ensure we are on master and it's clean
        git pull origin master || echo "Warning: Failed to update kernel tree, continuing with existing version."
    fi
    
    touch "$READY_FILE"
    echo "Background task: Kernel setup complete and marked as ready."
}

# Function to send test query after readiness
delayed_test_query() {
    local port="${SASHIKO__SERVER__PORT:-8080}"
    echo "Background task: Waiting for kernel readiness before sending test query..."
    
    # Wait for .ready file
    while [ ! -f "$READY_FILE" ]; do
        sleep 10
    done

    echo "Background task: Kernel is ready. Waiting 10 seconds for stability..."
    sleep 10

    echo "Background task: Sending test query to port $port..."
    curl -s -X POST "http://localhost:$port/api/submit" \
        -H "Content-Type: application/json" \
        -d '{"type": "remote", "sha": "1507f51255c9ff07d75909a84e7c0d7f3c4b2f49", "repo": "https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git"}' || echo "Test query failed"
    rm -f "$READY_FILE"
}

# Handle Cloud Run PORT environment variable mapping early
if [ -n "$PORT" ]; then
    echo "Mapping PORT=$PORT to SASHIKO__SERVER__PORT"
    export SASHIKO__SERVER__PORT="$PORT"
fi

# Start background tasks
setup_kernel &
sleep 1
delayed_test_query &
mkdir -p /data/db/

# IMPORTANT: Always return to /app before starting sashiko
cd /app

echo "Starting Sashiko..."

# Run sashiko. Logging to stdout/stderr is automatically captured by Cloud Run.
exec /usr/local/bin/sashiko "$@"
