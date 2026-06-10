.PHONY: archive clean check-dist sync-dist

WEB_SRC = ../ghostframe-web-client/dist
WEB_LOCAL = dist

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

archive: sync-dist
	go build -buildmode=c-archive -o libghostbridge.a .
	# This also generates libghostbridge.h

clean:
	rm -f libghostbridge.a libghostbridge.h
	rm -rf $(WEB_LOCAL)
