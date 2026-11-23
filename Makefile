# crabclip — forever clipboard history for macOS (Rust + AppKit)
APP       := crabclip
BUNDLE_ID := com.rod.crabclip
VERSION   := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
BUILD_NO  := $(shell date +%Y%m%d%H%M)
OUT       := build/$(APP).app
INSTALL   := /Applications/$(APP).app
# First Developer ID identity in the keychain, else ad-hoc ("-").
SIGN_ID   ?= $(or $(shell security find-identity -v -p codesigning 2>/dev/null | sed -n 's/.*"\(Developer ID Application:[^"]*\)".*/\1/p' | head -1),-)
COLLECTIVE_DIR := $(HOME)/Library/Containers/com.generation-loss.collective/Data/Library/Application Support/Collective

.DEFAULT_GOAL := help
.PHONY: help build bundle run test install uninstall import-collective import-clipy unload-collective release logs stats icons clean

help: ## Show this help
	@echo "crabclip $(VERSION) — forever clipboard history (menu bar app, ⌘⌥V)"
	@echo
	@echo "Usage: make <target>"
	@echo
	@awk 'BEGIN{FS=":.*## "} /^[a-zA-Z_-]+:.*## /{printf "  \033[1m%-18s\033[0m %s\n", $$1, $$2}' $(MAKEFILE_LIST)
	@echo
	@echo "Data:  ~/Library/Application Support/crabclip/crabclip.db (SQLite, zstd blobs, FTS5 trigram)"
	@echo "Logs:  ~/Library/Logs/crabclip.log"
	@echo "Sign:  SIGN_ID='$(SIGN_ID)' (default: first Developer ID identity, else ad-hoc \"-\")"

build: bundle ## Release build + signed build/crabclip.app

bundle:
	cargo build --release
	rm -rf "$(CURDIR)/$(OUT)"
	mkdir -p $(OUT)/Contents/MacOS $(OUT)/Contents/Resources $(OUT)/Contents/Library/LaunchAgents
	cp target/release/$(APP) $(OUT)/Contents/MacOS/$(APP)
	sed -e 's/__VERSION__/$(VERSION)/' -e 's/__BUILD__/$(BUILD_NO)/' bundle/Info.plist > $(OUT)/Contents/Info.plist
	cp bundle/com.rod.crabclip.agent.plist $(OUT)/Contents/Library/LaunchAgents/
	cp assets/AppIcon.icns assets/StatusIcon.png assets/StatusIcon@2x.png $(OUT)/Contents/Resources/
	codesign --force --options runtime --timestamp=none --sign "$(SIGN_ID)" $(OUT)
	codesign --verify --strict $(OUT) && echo "built $(OUT)"

run: build ## Build and run from build/ (takes over from a running copy, which quits cleanly)
	open -n $(OUT) --args --takeover

test: ## Run the test suite
	cargo test

install: build ## Install to /Applications and (re)start it; login agent keeps it running
	-@osascript -e 'tell application id "$(BUNDLE_ID)" to quit' 2>/dev/null; sleep 1; pkill -x $(APP) 2>/dev/null; sleep 1
	rm -rf "$(INSTALL)"
	cp -R $(OUT) "$(INSTALL)"
	@if launchctl print gui/$$(id -u)/com.rod.crabclip.agent >/dev/null 2>&1; then \
		launchctl kickstart -k gui/$$(id -u)/com.rod.crabclip.agent; echo "restarted login agent"; \
	else open "$(INSTALL)"; fi
	@echo "installed $(INSTALL)"

uninstall: ## Quit and remove /Applications/crabclip.app (keeps your history db)
	-@pkill -x $(APP)
	-launchctl bootout gui/$$(id -u)/com.rod.crabclip.agent 2>/dev/null
	rm -rf "$(INSTALL)"

import-collective: ## Import Collective.app's saved history (quit Collective first so it saves)
	target/release/$(APP) --import-collective

import-clipy: ## Import Clipy's history, snippet folders and shortcuts (FILE=snippets.xml for an export)
	target/release/$(APP) --import-clipy $(FILE)

unload-collective: ## Quit Collective (lets it save) and remove it from login items
	-osascript -e 'tell application "Collective" to quit'
	@for i in $$(seq 1 300); do pgrep -x Collective >/dev/null || break; sleep 1; done; \
	 pgrep -x Collective >/dev/null && echo "Collective still running (saving?)" || echo "Collective quit"
	-osascript -e 'tell application "System Events" to delete login item "Collective"'
	defaults write com.generation-loss.collective launchAtLogin -bool false

release: test build ## Tests, signed build, zip into dist/
	mkdir -p dist
	rm -f "$(CURDIR)/dist/$(APP)-$(VERSION).zip"
	ditto -c -k --keepParent $(OUT) dist/$(APP)-$(VERSION).zip
	@echo "dist/$(APP)-$(VERSION).zip"

logs: ## Follow the log
	tail -f ~/Library/Logs/crabclip.log

stats: ## Database stats
	target/release/$(APP) --stats

icons: ## Rebuild AppIcon.icns from assets/AppIcon.png
	rm -rf "$(CURDIR)/.agent/iconset"; mkdir -p .agent/iconset/AppIcon.iconset
	for s in 16 32 128 256 512; do \
		sips -z $$s $$s assets/AppIcon.png --out .agent/iconset/AppIcon.iconset/icon_$${s}x$${s}.png >/dev/null; \
		sips -z $$((s*2)) $$((s*2)) assets/AppIcon.png --out .agent/iconset/AppIcon.iconset/icon_$${s}x$${s}@2x.png >/dev/null; \
	done
	iconutil -c icns .agent/iconset/AppIcon.iconset -o assets/AppIcon.icns

clean: ## Remove build outputs
	cargo clean
	rm -rf "$(CURDIR)/build" "$(CURDIR)/dist"
