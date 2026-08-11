// A thin ABI-adapter shim between Plum's `extern "C"` surface
// (Int/Float/Bool/CStr/qualifying-struct only — no raw pointers, no
// 32-bit `float`, no `unsigned char`-sized struct fields) and raylib's
// real C API, which uses exactly those two things everywhere
// (`Vector2` is two 32-bit `float`s, `Color` is four `unsigned char`s).
// Every function here takes/returns only `long long` (Plum `Int`),
// `double` (Plum `Float`), `int` (Plum `Bool`, 0/1), or `const char *`
// (Plum `CStr`) — building the real raylib types internally before
// calling through. See ../../SPEC.md for the game this backs, and the
// plum repo's own README ("Editor support"/FFI section) for why this
// shim exists at all rather than binding raylib directly.
#include <raylib.h>

void raylib_init_window(long long width, long long height, const char *title) {
    InitWindow((int)width, (int)height, title);
}

void raylib_close_window(void) {
    CloseWindow();
}

int raylib_window_should_close(void) {
    return WindowShouldClose() ? 1 : 0;
}

void raylib_set_target_fps(long long fps) {
    SetTargetFPS((int)fps);
}

double raylib_get_frame_time(void) {
    return (double)GetFrameTime();
}

void raylib_begin_drawing(void) {
    BeginDrawing();
}

void raylib_end_drawing(void) {
    EndDrawing();
}

void raylib_clear_background(long long r, long long g, long long b, long long a) {
    ClearBackground((Color){ (unsigned char)r, (unsigned char)g, (unsigned char)b, (unsigned char)a });
}

// `x`/`y` are `double` here (not raylib's own `int centerX, centerY`
// DrawCircleLines takes) so that Plum's `Float` positions (every game
// entity's `position` is `Vec2 { x: Float, y: Float }`) never need a
// Float-to-Int conversion at the Plum call site — Plum has NO such
// conversion in either direction (see DESIGN.md/README) — the `(int)`
// truncation happens right here instead, the one place it's needed.
void raylib_draw_circle_lines(double x, double y, double radius, long long r, long long g, long long b, long long a) {
    DrawCircleLines((int)x, (int)y, (float)radius, (Color){ (unsigned char)r, (unsigned char)g, (unsigned char)b, (unsigned char)a });
}

void raylib_draw_circle_v(double x, double y, double radius, long long r, long long g, long long b, long long a) {
    DrawCircleV((Vector2){ (float)x, (float)y }, (float)radius, (Color){ (unsigned char)r, (unsigned char)g, (unsigned char)b, (unsigned char)a });
}

void raylib_draw_triangle_lines(
    double x1, double y1, double x2, double y2, double x3, double y3, long long r, long long g, long long b, long long a
) {
    DrawTriangleLines(
        (Vector2){ (float)x1, (float)y1 },
        (Vector2){ (float)x2, (float)y2 },
        (Vector2){ (float)x3, (float)y3 },
        (Color){ (unsigned char)r, (unsigned char)g, (unsigned char)b, (unsigned char)a }
    );
}

// `x`/`y`/`font_size` stay `long long` (real `Int`, not cast from a
// `Float`) — every caller in this game already has these as literal
// pixel/font-size integers or values derived from `raylib_measure_
// text`'s own `Int` return, never from a `Vec2`, so no Float-to-Int
// conversion is needed here at all (unlike `raylib_draw_circle_lines`
// above).
void raylib_draw_text(const char *text, long long x, long long y, long long font_size, long long r, long long g, long long b, long long a) {
    DrawText(text, (int)x, (int)y, (int)font_size, (Color){ (unsigned char)r, (unsigned char)g, (unsigned char)b, (unsigned char)a });
}

long long raylib_measure_text(const char *text, long long font_size) {
    return (long long)MeasureText(text, (int)font_size);
}

int raylib_is_key_down(long long key) {
    return IsKeyDown((int)key) ? 1 : 0;
}

int raylib_is_key_pressed(long long key) {
    return IsKeyPressed((int)key) ? 1 : 0;
}
