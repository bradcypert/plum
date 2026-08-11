BIN = asteroids

.PHONY: all run clean

all: $(BIN)

$(BIN): main.plum native/raylib_shim.c
	plum build . -o $(BIN) --link-lib raylib

run: $(BIN)
	./$(BIN)

clean:
	rm -f $(BIN)
