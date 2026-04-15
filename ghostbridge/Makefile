.PHONY: archive clean

archive:
	go build -buildmode=c-archive -o libghostbridge.a .
	# This also generates libghostbridge.h

clean:
	rm -f libghostbridge.a libghostbridge.h
