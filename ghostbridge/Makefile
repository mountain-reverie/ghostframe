.PHONY: archive archive-noweb clean check-dist sync-dist

WEB_SRC = ../ghostframe-web-client/dist
WEB_LOCAL = dist

# Where the c-archive (and its .h, which go writes beside it) lands.
# ghostframe-tsnet/build.rs points this at Cargo's OUT_DIR so the two
# archive flavours never share a file and the build never writes into the
# directory Cargo watches. A manual `make archive` keeps the local default.
OUT ?= libghostbridge.a

check-dist:
	@test -f $(WEB_SRC)/index.html || ( \
	  echo "error: ghostframe-web-client/dist not built" >&2; \
	  echo "  ghostbridge //go:embeds the SPA at compile time;" >&2; \
	  echo "  run: just build-web" >&2; \
	  echo "  or:  cd ghostframe-web-client && npm ci && npm run build" >&2; \
	  exit 1 )

# Go's //go:embed does not follow symlinks and does not allow `..` in
# patterns, so we copy the SPA tree into a sibling dist/ that embed can
# read. rsync --delete keeps stale files from accumulating across builds.
sync-dist: check-dist
	@mkdir -p $(WEB_LOCAL)
	@rsync -a --delete $(WEB_SRC)/ $(WEB_LOCAL)/

# The server archive: embeds the SPA, so it needs dist/ built first.
archive: sync-dist
	go build -buildmode=c-archive -o $(OUT) .
	# This also generates libghostbridge.h

# The native-client archive. Same tsnet surface, no //go:embed, so no npm
# and no vite -- which is the whole point: `cargo build -p ghostframe-cli`
# used to require a web client build it never serves a byte of. Selected by
# ghostframe-tsnet/build.rs when the `web-embed` feature is off.
#
# build.rs gives each feature set its own OUT, so this and `archive` never
# overwrite each other's archive.
archive-noweb:
	go build -tags noweb -buildmode=c-archive -o $(OUT) .

clean:
	rm -f libghostbridge.a libghostbridge.h
	rm -rf $(WEB_LOCAL)
