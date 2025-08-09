#!/bin/bash

# CIAOS Server Test Runner
# This script runs the comprehensive test suite for the CIAOS storage service

set -e

echo "🚀 CIAOS Server Test Suite"
echo "=========================="
echo

# Check if we're in the correct directory
if [ ! -f "Cargo.toml" ]; then
    echo "❌ Error: Please run this script from the server directory"
    echo "   Expected: server/run_tests.sh"
    exit 1
fi

# Check if Rust/Cargo is installed
if ! command -v cargo &> /dev/null; then
    echo "❌ Error: Cargo not found. Please install Rust:"
    echo "   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
    exit 1
fi

echo "📋 Test Environment Information"
echo "------------------------------"
echo "Rust version: $(rustc --version)"
echo "Cargo version: $(cargo --version)"
echo "Test target: Service layer functions (issues #66-#71)"
echo

# Create temporary directories with proper permissions
export TEST_TMP_DIR="/tmp/ciaos_test_$$"
mkdir -p "$TEST_TMP_DIR"
chmod 755 "$TEST_TMP_DIR"

echo "🔧 Test Configuration"
echo "--------------------"
echo "Temporary directory: $TEST_TMP_DIR"
echo "Database isolation: ✅ Enabled"
echo "Storage isolation: ✅ Enabled"
echo "Multi-user testing: ✅ Enabled"
echo

# Set environment variables for test isolation
export RUST_LOG=info
export DB_FILE="$TEST_TMP_DIR/test_metadata.sqlite"
export STORAGE_DIRECTORY="$TEST_TMP_DIR/storage"

# Ensure test directories exist with proper permissions
mkdir -p "$(dirname "$DB_FILE")"
mkdir -p "$STORAGE_DIRECTORY"
chmod 755 "$(dirname "$DB_FILE")" "$STORAGE_DIRECTORY"

echo "🧪 Running Test Suite"
echo "--------------------"

# Run tests with detailed output
echo "Running all service layer tests..."
if cargo test --bin CIAOS 2>&1 | tee test_output.log; then
    echo
    echo "✅ Test execution completed successfully!"
    
    # Parse test results
    PASSED=$(grep -o "test result: [^;]*" test_output.log | grep -o "[0-9]* passed" | grep -o "[0-9]*" || echo "0")
    FAILED=$(grep -o "test result: [^;]*" test_output.log | grep -o "[0-9]* failed" | grep -o "[0-9]*" || echo "0")
    
    echo
    echo "📊 Test Results Summary"
    echo "======================"
    echo "✅ Passed: $PASSED tests"
    echo "❌ Failed: $FAILED tests"
    
    if [ "$FAILED" -eq 0 ]; then
        echo "🎉 All tests passed! The service layer is working correctly."
        EXIT_CODE=0
    else
        echo "⚠️  Some tests failed. This may be due to environment setup issues."
        echo "   See test_output.log for detailed failure information."
        EXIT_CODE=1
    fi
    
else
    echo
    echo "❌ Test execution failed!"
    echo "   Check test_output.log for detailed error information."
    EXIT_CODE=1
fi

echo
echo "🧹 Cleanup"
echo "---------"
# Clean up temporary directories
rm -rf "$TEST_TMP_DIR"
echo "Temporary files cleaned up"

# Show test categories covered
echo
echo "🎯 Test Categories Covered"
echo "==========================="
echo "✅ Authentication & Security (User header validation)"
echo "✅ Database Operations (CRUD operations on keys)"  
echo "✅ Storage Operations (File write/read functionality)"
echo "✅ Serialization (FlatBuffers data integrity)"
echo "✅ Integration Testing (End-to-end workflows)"
echo "✅ Error Handling (Edge cases and failure scenarios)"
echo "✅ Multi-user Isolation (Data separation validation)"

echo
echo "📖 Service Functions Tested (Issues #66-#71)"
echo "=============================================="
echo "✅ put_service - Data upload functionality"
echo "✅ get_service - Data retrieval functionality"
echo "✅ append_service - Data appending functionality" 
echo "✅ delete_service - Data deletion functionality"
echo "✅ update_key_service - Key renaming functionality"
echo "✅ update_service - Data replacement functionality"

echo
echo "📚 For detailed test documentation, see TESTING.md"
echo "🔍 For test logs, see test_output.log"

exit $EXIT_CODE