#!/bin/bash
# switch_profile.sh - Switch between Development and Distribution signing profiles
# Usage: ./scripts/switch_profile.sh [dev|dist|status]

set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Profile paths
DIST_PROFILE="./dist.provisionprofile"
DEV_PROFILE="./dev.provisionprofile"
ACTIVE_PROFILE="./embedded.provisionprofile"

# Signing identities
DEV_IDENTITY="EA4C9CD3EDDE09F48B81734AEB4900D07F67193C"
DIST_IDENTITY="Apple Distribution: DUSTIN ALAN MOORE (ABXVW6PWCW)"

get_profile_name() {
    local profile_path="$1"
    if [ -f "$profile_path" ]; then
        security cms -D -i "$profile_path" 2>/dev/null | grep -A1 "<key>Name</key>" | tail -1 | sed 's/.*<string>\(.*\)<\/string>.*/\1/' | xargs
    else
        echo "NOT FOUND"
    fi
}

get_profile_type() {
    local profile_path="$1"
    if [ -f "$profile_path" ]; then
        local type=$(security cms -D -i "$profile_path" 2>/dev/null | grep -A1 "ProfileDistributionType" | tail -1 | sed 's/.*<string>\(.*\)<\/string>.*/\1/' | xargs)
        if [ -z "$type" ]; then
            # Check for ProvisionedDevices (indicates dev profile)
            if security cms -D -i "$profile_path" 2>/dev/null | grep -q "ProvisionedDevices"; then
                echo "DEVELOPMENT"
            else
                echo "UNKNOWN"
            fi
        else
            echo "$type"
        fi
    else
        echo "N/A"
    fi
}

show_status() {
    echo ""
    echo -e "${BLUE}=== Current Signing Configuration ===${NC}"
    echo ""
    
    # Active profile
    local active_name=$(get_profile_name "$ACTIVE_PROFILE")
    local active_type=$(get_profile_type "$ACTIVE_PROFILE")
    
    if [ "$active_type" = "STORE" ]; then
        echo -e "Active Profile:  ${GREEN}$active_name${NC} (Distribution)"
        echo -e "                 ${YELLOW}⚠️  Cannot run locally - for App Store upload only${NC}"
    elif [ "$active_type" = "DEVELOPMENT" ]; then
        echo -e "Active Profile:  ${GREEN}$active_name${NC} (Development)"
        echo -e "                 ${GREEN}✓ Can run locally for testing${NC}"
    else
        echo -e "Active Profile:  ${RED}$active_name${NC} ($active_type)"
    fi
    echo ""
    
    # Available profiles
    echo "Available Profiles:"
    if [ -f "$DEV_PROFILE" ]; then
        local dev_name=$(get_profile_name "$DEV_PROFILE")
        echo -e "  dev.provisionprofile:  ${GREEN}$dev_name${NC}"
    else
        echo -e "  dev.provisionprofile:  ${RED}NOT FOUND${NC}"
    fi
    
    if [ -f "$DIST_PROFILE" ]; then
        local dist_name=$(get_profile_name "$DIST_PROFILE")
        echo -e "  dist.provisionprofile: ${GREEN}$dist_name${NC}"
    else
        echo -e "  dist.provisionprofile: ${RED}NOT FOUND${NC}"
    fi
    echo ""
    
    # Signing identities
    echo "Available Signing Identities:"
    security find-identity -v -p codesigning 2>/dev/null | grep -E "(Apple Development|Apple Distribution)" | while read line; do
        echo "  $line"
    done
    echo ""
}

switch_to_dev() {
    echo ""
    echo -e "${BLUE}=== Switching to DEVELOPMENT Profile ===${NC}"
    
    if [ ! -f "$DEV_PROFILE" ]; then
        echo -e "${RED}Error: dev.provisionprofile not found!${NC}"
        echo "Please copy your development provisioning profile to ./dev.provisionprofile"
        exit 1
    fi
    
    # Backup current if it's distribution
    local current_type=$(get_profile_type "$ACTIVE_PROFILE")
    if [ "$current_type" = "STORE" ] && [ ! -f "$DIST_PROFILE" ]; then
        echo "Backing up current distribution profile to dist.provisionprofile..."
        cp "$ACTIVE_PROFILE" "$DIST_PROFILE"
    fi
    
    # Switch
    cp "$DEV_PROFILE" "$ACTIVE_PROFILE"
    
    # Clean old build
    if [ -d "target/dx/Hobbes/release/macos/Hobbes.app" ]; then
        echo "Cleaning old .app bundle..."
        rm -rf "target/dx/Hobbes/release/macos/Hobbes.app"
    fi
    
    echo ""
    echo -e "${GREEN}✓ Switched to Development profile${NC}"
    echo ""
    echo "Build command:"
    echo -e "  ${YELLOW}HOBBES_SIGNING_ID=\"$DEV_IDENTITY\" ./scripts/build_release.sh${NC}"
    echo ""
    echo "Or run directly:"
    echo -e "  ${YELLOW}./scripts/build_dev.sh${NC}"
    echo ""
}

switch_to_dist() {
    echo ""
    echo -e "${BLUE}=== Switching to DISTRIBUTION Profile ===${NC}"
    
    if [ ! -f "$DIST_PROFILE" ]; then
        echo -e "${RED}Error: dist.provisionprofile not found!${NC}"
        echo "Please copy your distribution provisioning profile to ./dist.provisionprofile"
        exit 1
    fi
    
    # Backup current if it's development
    local current_type=$(get_profile_type "$ACTIVE_PROFILE")
    if [ "$current_type" = "DEVELOPMENT" ] && [ ! -f "$DEV_PROFILE" ]; then
        echo "Backing up current development profile to dev.provisionprofile..."
        cp "$ACTIVE_PROFILE" "$DEV_PROFILE"
    fi
    
    # Switch
    cp "$DIST_PROFILE" "$ACTIVE_PROFILE"
    
    # Clean old build
    if [ -d "target/dx/Hobbes/release/macos/Hobbes.app" ]; then
        echo "Cleaning old .app bundle..."
        rm -rf "target/dx/Hobbes/release/macos/Hobbes.app"
    fi
    
    echo ""
    echo -e "${GREEN}✓ Switched to Distribution profile${NC}"
    echo ""
    echo "Build command:"
    echo -e "  ${YELLOW}./scripts/build_release.sh && ./scripts/package_release.sh${NC}"
    echo ""
    echo -e "${YELLOW}⚠️  Remember: Distribution builds cannot run locally!${NC}"
    echo ""
}

show_help() {
    echo ""
    echo "Usage: $0 [command]"
    echo ""
    echo "Commands:"
    echo "  dev      Switch to Development profile (for local testing)"
    echo "  dist     Switch to Distribution profile (for App Store/TestFlight)"
    echo "  status   Show current configuration (default)"
    echo "  help     Show this help message"
    echo ""
    echo "Examples:"
    echo "  $0 dev      # Switch to dev, then build for local testing"
    echo "  $0 dist     # Switch to dist, then build for upload"
    echo ""
}

# Main
case "${1:-status}" in
    dev|development)
        switch_to_dev
        ;;
    dist|distribution|release)
        switch_to_dist
        ;;
    status|info)
        show_status
        ;;
    help|-h|--help)
        show_help
        ;;
    *)
        echo -e "${RED}Unknown command: $1${NC}"
        show_help
        exit 1
        ;;
esac
