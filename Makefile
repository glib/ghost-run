# Ghost Run: build and install for the current user.
#   make install            # ~/.local/bin, launcher + icon in your app menu
#   make install PREFIX=... # elsewhere
#   make uninstall
#
# Inside a distrobox, the launcher starts the app through `distrobox enter`,
# because the binary uses the box's GTK libraries (CONTAINER_ID is set by
# distrobox; override with BOX=name).

PREFIX ?= $(HOME)/.local
APP_ID := io.github.glib.GhostRun
BOX ?= $(CONTAINER_ID)
BIN := $(PREFIX)/bin/ghost-run
EXEC := $(if $(BOX),distrobox enter $(BOX) -- $(BIN),$(BIN))

.PHONY: build check install uninstall
build:
	cargo build --release --locked -p ghost-run -p timer-cli

check:
	cargo fmt --all --check
	cargo clippy --all-targets --locked -- -D warnings
	cargo test --locked

install: build
	install -Dm755 target/release/ghost-run $(DESTDIR)$(BIN)
	install -Dm755 target/release/rtimer $(DESTDIR)$(PREFIX)/bin/rtimer
	install -d $(DESTDIR)$(PREFIX)/share/applications
	sed 's|@EXEC@|$(EXEC)|' packaging/$(APP_ID).desktop > $(DESTDIR)$(PREFIX)/share/applications/$(APP_ID).desktop
	install -Dm644 packaging/$(APP_ID).svg $(DESTDIR)$(PREFIX)/share/icons/hicolor/scalable/apps/$(APP_ID).svg
	@$(if $(DESTDIR),true,sh packaging/refresh-menus.sh $(PREFIX)/share/applications $(PREFIX)/share/icons)
	@echo "Installed. Launcher runs: $(EXEC)"

uninstall:
	rm -f $(DESTDIR)$(BIN) $(DESTDIR)$(PREFIX)/bin/rtimer \
	  $(DESTDIR)$(PREFIX)/share/applications/$(APP_ID).desktop \
	  $(DESTDIR)$(PREFIX)/share/icons/hicolor/scalable/apps/$(APP_ID).svg
	@$(if $(DESTDIR),true,sh packaging/refresh-menus.sh $(PREFIX)/share/applications $(PREFIX)/share/icons)
