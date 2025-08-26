#!/usr/bin/env bash

# Exit immediately if a command exits with a non-zero status.
set -e

echo "--- Starting RooFlow config setup (with ConPort strategy update) ---"

# Check for Git command
if ! command -v git &> /dev/null; then
    echo "Error: git is not found in your PATH."
    echo "Please install Git using your distribution's package manager (e.g., sudo apt install git, sudo yum install git)."
    exit 1
else
    echo "Found git executable."
fi

# Check for Python 3
if ! command -v python3 &> /dev/null; then
    echo "Error: python3 is not found in your PATH."
    echo "Please install Python 3 (https://www.python.org/downloads/)."
    exit 1
else
    echo "Found python3 executable."
fi

# Debugging: Determine which python3 is being used
echo "Debugging: Which python3 is being used by the script?"
command -v python3
python3 --version

# Debugging: Determine which pip is being used and its associated Python
echo "Debugging: Which pip is being used?"
command -v pip
pip --version

# Check for PyYAML library
if ! python3 -c "import yaml" &> /dev/null; then
    echo "Error: PyYAML library is not found for Python 3."
    echo "Please install it using: python3 -m pip install pyyaml"
    exit 1
else
    echo "Found PyYAML library."
fi

# Define a temporary directory name for clarity
CLONE_DIR="RooFlow_temp_$$" # Using $$ for process ID to add uniqueness

# Clone the repository (shallow clone for efficiency)
echo "Cloning RooFlow repository into $CLONE_DIR..."
git clone --depth 1 https://github.com/GreatScottyMac/RooFlow "$CLONE_DIR"

# --- MODIFIED COPY SECTION START ---
echo "Copying specific configuration items..."

# 1. Copy .roo directory (recursively)
echo "Copying .roo directory..."
cp -r "$CLONE_DIR/config/.roo" ./

# 2. Copy specific config files
echo "Copying .roomodes, Python script, and ConPort strategy..."
cp "$CLONE_DIR/config/.roomodes" ./
cp "$CLONE_DIR/config/generate_mcp_yaml.py" ./
cp "$CLONE_DIR/config/roo_code_conport_strategy" ./ # Added copy for ConPort strategy

# --- MODIFIED COPY SECTION END ---

# Check if essential files exist before running Python script
if [ ! -d ".roo" ]; then
    echo "Error: .roo directory not found after specific copy. Setup failed."
    rm -rf "$CLONE_DIR" # Clean up clone dir before exiting
    exit 1
fi
if [ ! -f "generate_mcp_yaml.py" ]; then
     echo "Error: generate_mcp_yaml.py not found after specific copy. Setup failed."
     rm -rf "$CLONE_DIR" # Clean up clone dir before exiting
     exit 1
fi
if [ ! -f "roo_code_conport_strategy" ]; then # Check for strategy file
     echo "Error: roo_code_conport_strategy not found after specific copy. Setup failed."
     rm -rf "$CLONE_DIR" # Clean up clone dir before exiting
     exit 1
fi

# Run the Python script to process templates
echo "Running Python script to process templates..."
if [[ "$(uname)" == "Darwin" ]]; then OS_VAL="macOS $(sw_vers -productVersion)"; else OS_VAL=$(uname -s -r); fi
SHELL_VAL="bash"
HOME_VAL=$(echo "$HOME")
WORKSPACE_VAL=$(pwd)

python3 generate_mcp_yaml.py --os "$OS_VAL" --shell "$SHELL_VAL" --home "$HOME_VAL" --workspace "$WORKSPACE_VAL"

# --- EMBEDDED PROMPT UPDATE LOGIC ---
echo "--- Starting Roo prompt update with ConPort strategy ---"

# Define paths for update logic (relative to current dir, which is workspace root)
UPD_ROO_DIR=".roo"
UPD_STRATEGY_FILE="roo_code_conport_strategy" # This was copied to CWD

# Target prompt files for update logic (relative to UPD_ROO_DIR)
UPD_ARCHITECT_PROMPT="system-prompt-flow-architect"
UPD_ASK_PROMPT="system-prompt-flow-ask"
UPD_CODE_PROMPT="system-prompt-flow-code"
UPD_DEBUG_PROMPT="system-prompt-flow-debug"
UPD_ORCHESTRATOR_PROMPT="system-prompt-flow-orchestrator"

# Error check for update logic's required files
if [ ! -d "$UPD_ROO_DIR" ]; then
  echo "Error (Update Logic): Directory '$UPD_ROO_DIR' not found."
  # Main script already checks for .roo, but good to be defensive if this part is ever isolated
  rm -rf "$CLONE_DIR"
  exit 1
fi

if [ ! -f "$UPD_STRATEGY_FILE" ]; then
  echo "Error (Update Logic): Strategy file '$UPD_STRATEGY_FILE' not found in current path."
  rm -rf "$CLONE_DIR"
  exit 1
fi

# Function to process files that need replacement (from update_roo_prompts.sh)
process_replacement() {
  local target_file_path="$1"
  local temp_file=$(mktemp)

  if [ ! -f "$target_file_path" ]; then
    echo "Warning (Update Logic): Target file '$target_file_path' not found. Skipping."
    rm "$temp_file" # Clean up temp file if target doesn't exist
    return
  fi

  echo "Processing (Update Logic) $target_file_path for replacement..."
  line_num=$(grep -n "^memory_bank_strategy:" "$target_file_path" | cut -d: -f1)

  if [ -z "$line_num" ]; then
    echo "Warning (Update Logic): 'memory_bank_strategy:' not found in '$target_file_path'. Skipping replacement."
    rm "$temp_file"
    return
  fi

  head -n $((line_num - 1)) "$target_file_path" > "$temp_file"
  cat "$UPD_STRATEGY_FILE" >> "$temp_file"
  mv "$temp_file" "$target_file_path"
  echo "Updated (Update Logic) '$target_file_path'."
}

# Function to process the file that needs deletion (from update_roo_prompts.sh)
process_deletion() {
  local target_file_path="$1"
  local temp_file=$(mktemp)

  if [ ! -f "$target_file_path" ]; then
    echo "Warning (Update Logic): Target file '$target_file_path' not found. Skipping."
    rm "$temp_file" # Clean up temp file
    return
  fi

  echo "Processing (Update Logic) $target_file_path for deletion..."
  line_num=$(grep -n "^memory_bank_strategy:" "$target_file_path" | cut -d: -f1)

  if [ -z "$line_num" ]; then
    echo "Warning (Update Logic): 'memory_bank_strategy:' not found in '$target_file_path'. Skipping deletion."
    rm "$temp_file"
    return
  fi

  head -n $((line_num - 1)) "$target_file_path" > "$temp_file"
  mv "$temp_file" "$target_file_path"
  echo "Updated (Update Logic) '$target_file_path' (section deleted)."
}

# Main processing for prompt updates
process_replacement "$UPD_ROO_DIR/$UPD_ARCHITECT_PROMPT"
process_replacement "$UPD_ROO_DIR/$UPD_CODE_PROMPT"
process_replacement "$UPD_ROO_DIR/$UPD_DEBUG_PROMPT"
process_deletion "$UPD_ROO_DIR/$UPD_ASK_PROMPT"
process_replacement "$UPD_ROO_DIR/$UPD_ORCHESTRATOR_PROMPT"

echo "--- Roo prompt update with ConPort strategy completed ---"
# --- END EMBEDDED PROMPT UPDATE LOGIC ---

# Clean up the strategy file from the workspace root
if [ -f "$UPD_STRATEGY_FILE" ]; then
  echo "Cleaning up $UPD_STRATEGY_FILE from workspace root..."
  rm -f "$UPD_STRATEGY_FILE"
fi

# --- MODIFIED CLEANUP SECTION START ---
echo "Cleaning up temporary clone directory ($CLONE_DIR)..."
rm -rf "$CLONE_DIR" # Remove the cloned repo directory
# --- MODIFIED CLEANUP SECTION END ---

echo "--- RooFlow config setup (with ConPort strategy update) complete ---"
exit 0