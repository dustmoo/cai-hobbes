import sys
import json
import yaml
import re
import os
import argparse
import glob
import codecs

# --- YAML Custom Representers ---
def represent_dict_block(dumper, data):
    return dumper.represent_mapping('tag:yaml.org,2002:map', data, flow_style=False)
yaml.add_representer(dict, represent_dict_block, Dumper=yaml.SafeDumper)

def represent_list_block(dumper, data):
    return dumper.represent_sequence('tag:yaml.org,2002:seq', data, flow_style=False)
yaml.add_representer(list, represent_list_block, Dumper=yaml.SafeDumper)

# --- Helper Functions ---
def extract_balanced_json(text):
    """Finds the first '{' and extracts the substring until the matching '}'."""
    start_index = text.find('{')
    if start_index == -1: return None
    brace_level = 0
    for i, char in enumerate(text[start_index:]):
        if char == '{': brace_level += 1
        elif char == '}':
            brace_level -= 1
            if brace_level == 0: return text[start_index : start_index + i + 1]
    return None

def parse_mcp_servers_md(markdown_content: str) -> list:
    """
    Parses MCP server definitions from markdown content.
    Returns a list of server dictionaries.
    """
    servers = []
    markdown_content = markdown_content.replace('\r\n', '\n') # Normalize line endings
    # --- Define Delimiters ---
    start_delimiter = "\n\nWhen a server is connected, you can use the server's tools via the `use_mcp_tool` tool, and access the server's resources via the `access_mcp_resource` tool.\n\n" # Adjusted leading newline
    end_section_delimiter = "\n## Creating an MCP Server"

    # --- Extract Relevant Section ---
    start_index = markdown_content.find(start_delimiter)
    if start_index == -1:
        print("Error: Start delimiter not found in source markdown. Cannot extract MCP section.", file=sys.stderr)
        return []
    start_index += len(start_delimiter)
    end_index = markdown_content.find(end_section_delimiter, start_index)
    if end_index == -1:
        print(f"Warning: End section delimiter ('{end_section_delimiter.strip()}') not found after start delimiter. Processing until end of content.", file=sys.stderr)
        mcp_section_content = markdown_content[start_index:]
    else:
        mcp_section_content = markdown_content[start_index:end_index]

    # --- Parse Servers within the Extracted Section ---
    server_matches = re.finditer(
        r'^##\s*(?P<name>.+?)\s+\((?P<command>.+?)\)\s*\n(?P<rest>.*?)(?=^##\s|\Z)',
        mcp_section_content, re.MULTILINE | re.DOTALL
    )
    server_found = False
    for match in server_matches:
        server_found = True
        server_data = match.groupdict()
        server_name = server_data['name'].strip()
        server_command = server_data['command'].strip().strip('`')
        server_block_content = server_data['rest'].strip()

        # Description Extraction
        server_description = ""
        first_heading_pos = server_block_content.find('###')
        end_slice = first_heading_pos if first_heading_pos != -1 else len(server_block_content)
        desc_text = server_block_content[0:end_slice].strip()
        if desc_text and not desc_text.startswith("###"):
            server_description = ' '.join(line.strip() for line in desc_text.splitlines() if line.strip())

        current_server = {'name': server_name, 'command': server_command, 'description': server_description, 'tools': [], 'resources': []}

        # Tools Extraction
        tools_section_match = re.search(r'### Available Tools\s*(.*?)(?:### Direct Resources|\Z)', server_block_content, re.DOTALL | re.IGNORECASE)
        if tools_section_match:
            tools_content = tools_section_match.group(1).strip()
            tool_blocks = re.split(r'\n\s*-\s+(?=[\w_-]+:)', '\n' + tools_content)
            for block in tool_blocks:
                block = block.strip()
                if not block: continue
                name_match = re.match(r'^-?\s*([\w_-]+):', block)
                if not name_match: continue
                tool_name = name_match.group(1)
                block_after_name = re.sub(r'^-?\s*' + re.escape(tool_name) + r':\s*', '', block, count=1).strip()
                schema_marker = 'Input Schema:'
                schema_marker_pos = block_after_name.find(schema_marker)
                if schema_marker_pos != -1:
                    tool_description_raw = block_after_name[:schema_marker_pos].strip()
                    schema_text_part = block_after_name[schema_marker_pos + len(schema_marker):]
                    tool_description = ' '.join(line.strip() for line in tool_description_raw.splitlines() if line.strip())
                    schema_str = extract_balanced_json(schema_text_part)
                    if schema_str:
                        try:
                            input_schema_dict = json.loads(schema_str)
                            current_server['tools'].append({'name': tool_name, 'description': tool_description, 'input_schema': input_schema_dict})
                        except json.JSONDecodeError as e:
                            print(f"Warning: JSON parse error for tool '{tool_name}' in server '{server_name}': {e}", file=sys.stderr)
                    else:
                        print(f"Warning: Could not extract balanced JSON schema for tool '{tool_name}' in server '{server_name}'.", file=sys.stderr)
                else:
                    tool_description = ' '.join(line.strip() for line in block_after_name.splitlines() if line.strip())
                    if tool_description:
                        current_server['tools'].append({'name': tool_name, 'description': tool_description, 'input_schema': {}})

        # Resources Extraction
        resources_section_match = re.search(r'### Direct Resources\s*(.*?)(?:\Z)', server_block_content, re.DOTALL | re.IGNORECASE)
        if resources_section_match:
            resources_content = resources_section_match.group(1).strip()
            resource_matches = re.finditer(r'-\s*(?P<uri>.+?)\s+\((?P<description>.*?)\):\s*.*', resources_content)
            count = 0
            for r_match in resource_matches:
                res_dict = r_match.groupdict()
                current_server['resources'].append({'uri': res_dict['uri'].strip(), 'description': res_dict['description'].strip()})
                count += 1
            if count == 0 and resources_content.strip() and resources_content.strip().startswith('-'):
                print(f"Warning: Found 'Direct Resources' section for server '{server_name}' but could not parse entries.", file=sys.stderr)

        servers.append(current_server)

    if not server_found and mcp_section_content.strip():
        print(f"Warning: No servers found matching '## name (command)' pattern within extracted MCP section.", file=sys.stderr)

    return servers

def generate_mcp_yaml(servers: list) -> str | None:
    """Generates the indented MCP YAML string from a list of server dicts."""
    if not servers:
        return "    servers: []" # Explicitly return empty list if no servers found

    try:
        output_data = {'servers': servers}
        yaml_output = yaml.dump(output_data, Dumper=yaml.SafeDumper, default_flow_style=None, sort_keys=False, indent=2, width=2000)
        # Prepend 4 spaces to each line for desired base indentation
        indented_yaml_output = "\n".join(["    " + line for line in yaml_output.splitlines()])
        # Escape backslashes for regex replacement in target files
        escaped_yaml_output = indented_yaml_output.replace('\\', '\\\\')
        return escaped_yaml_output
    except yaml.YAMLError as e:
        print(f"Error generating YAML: {e}", file=sys.stderr)
        return None

def process_target_file(file_path: str, args: argparse.Namespace, mcp_yaml_content: str | None):
    """Reads target file, performs all substitutions, and writes back."""
    print(f"Processing: {file_path}")
    try:
        # Read with UTF-8, handle potential BOM
        with codecs.open(file_path, 'r', encoding='utf-8-sig') as f:
            content = f.read()

        # 1. Basic Placeholders (Match bracketed format used in templates)
        content = content.replace("[OS_PLACEHOLDER]", args.os or "Unknown OS")
        content = content.replace("[SHELL_PLACEHOLDER]", args.shell or "Unknown Shell")
        # Use arguments directly for replacement
        content = content.replace("[HOME_PLACEHOLDER]", args.home or 'Unknown Home')
        content = content.replace("[WORKSPACE_PLACEHOLDER]", args.workspace or 'Unknown Workspace')

        # 2. MCP Block Injection/Overwrite
        placeholder_pattern = r'#\s*\[CONNECTED_MCP_SERVERS\]' # Python regex
        start_marker = '# MCP Server list injected by script'
        end_marker = '# End MCP Server list'

        # Pattern to find EITHER the placeholder OR the existing injected block
        escaped_start_marker = re.escape(start_marker)
        escaped_end_marker = re.escape(end_marker)
        # DOTALL (?s) allows . to match newline, MULTILINE (?m) allows ^ to match start of line
        existing_block_pattern = rf"(^[ \t]*{escaped_start_marker}.*?^[ \t]*{escaped_end_marker}[ \t]*\r?\n?)"
        placeholder_line_pattern = rf"(^[ \t]*{placeholder_pattern}[ \t]*\r?\n?)"
        combined_pattern = rf"{existing_block_pattern}|{placeholder_line_pattern}"

        injection_possible = mcp_yaml_content is not None

        match = re.search(combined_pattern, content, re.MULTILINE | re.DOTALL)

        if match:
            if injection_possible:
                print(f"  Injecting/Overwriting MCP block in {os.path.basename(file_path)}...")
                replacement_block = f"{start_marker}\n{mcp_yaml_content}\n{end_marker}"
                content = re.sub(combined_pattern, replacement_block, content, count=1, flags=re.MULTILINE | re.DOTALL)
            else:
                print(f"  Placeholder/Block found but no MCP content generated. Skipping injection in {os.path.basename(file_path)}.")
                # Optionally remove old block even if injection fails? For now, leave it.
                # content = re.sub(combined_pattern, "", content, count=1, flags=re.MULTILINE | re.DOTALL)
        else:
             print(f"  Placeholder or existing block not found in {os.path.basename(file_path)}. Skipping injection.")


        # Write back with UTF-8 without BOM
        with codecs.open(file_path, 'w', encoding='utf-8') as f:
            f.write(content)
        print(f"  Completed: {os.path.basename(file_path)}")

    except FileNotFoundError:
        print(f"Error: Target file not found: {file_path}", file=sys.stderr)
    except Exception as e:
        print(f"Error processing file {file_path}: {e}", file=sys.stderr)


# --- Main Execution ---
if __name__ == "__main__":
    parser = argparse.ArgumentParser(description="Generate MCP YAML and inject into system prompts.")
    parser.add_argument("--os", required=True, help="Operating System name")
    parser.add_argument("--shell", required=True, help="Default shell name")
    parser.add_argument("--home", required=True, help="Home directory path")
    parser.add_argument("--workspace", required=True, help="Workspace directory path")
    args = parser.parse_args()

    cwd = args.workspace # Use workspace passed from installer as CWD
    source_md_path = os.path.join(cwd, "system_prompt.md")
    roo_dir_path = os.path.join(cwd, ".roo")

    print(f"Workspace: {cwd}")
    print(f"Source MD: {source_md_path}")
    print(f"Target Dir: {roo_dir_path}")

    mcp_yaml_content = None
    if not os.path.exists(source_md_path):
        print(f"Warning: Source markdown file not found at {source_md_path}. Skipping MCP generation.", file=sys.stderr)
    else:
        try:
            print(f"Reading source markdown: {source_md_path}")
            with codecs.open(source_md_path, 'r', encoding='utf-8-sig') as f:
                md_content = f.read()
            print("Parsing MCP servers...")
            parsed_servers = parse_mcp_servers_md(md_content)
            print(f"Found {len(parsed_servers)} server(s).")
            if parsed_servers is not None: # parse_mcp_servers_md returns [] on error/not found
                print("Generating MCP YAML...")
                mcp_yaml_content = generate_mcp_yaml(parsed_servers)
                if mcp_yaml_content:
                    print("MCP YAML generated successfully.")
                else:
                    print("Warning: Failed to generate MCP YAML.", file=sys.stderr)
            else:
                 print("Warning: Parsing MCP servers failed.", file=sys.stderr)

        except Exception as e:
            print(f"Error during MCP generation: {e}", file=sys.stderr)

    if not os.path.isdir(roo_dir_path):
        print(f"Error: Target .roo directory not found at {roo_dir_path}", file=sys.stderr)
        sys.exit(1)

    print("Processing target prompt files...")
    target_files = glob.glob(os.path.join(roo_dir_path, 'system-prompt-*'))

    if not target_files:
         print(f"Warning: No 'system-prompt-*' files found in {roo_dir_path}", file=sys.stderr)

    for target_file in target_files:
        if os.path.isfile(target_file): # Ensure it's a file
             process_target_file(target_file, args, mcp_yaml_content)

    print("Processing complete.")