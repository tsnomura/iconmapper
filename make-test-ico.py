# Generate a simple multi-size test .ico (red/green/blue squares) for iconmapper.
import struct
import sys


def make_image(size, r, g, b):
    # 32-bit BGRA, bottom-up rows
    px = bytes([b, g, r, 255]) * (size * size)
    # AND mask: 1bpp, all zero (fully opaque), rows padded to 32 bits
    row_bytes = ((size + 31) // 32) * 4
    and_mask = b"\x00" * (row_bytes * size)
    bih = struct.pack(
        "<IiiHHIIiiII",
        40,          # biSize
        size,        # biWidth
        size * 2,    # biHeight (XOR + AND)
        1,           # biPlanes
        32,          # biBitCount
        0,           # biCompression
        len(px) + len(and_mask),
        0, 0, 0, 0,
    )
    return bih + px + and_mask


def main():
    if len(sys.argv) == 5:
        # single color: make-test-ico.py OUT R G B
        dest = sys.argv[1]
        r, g, b = int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4])
        sizes = [16, 24, 32, 48, 64]
        images = [(s, make_image(s, r, g, b)) for s in sizes]
        n = len(images)
        out = struct.pack("<HHH", 0, 1, n)
        offset = 6 + 16 * n
        entries = b""
        for s, data in images:
            w = s if s < 256 else 0
            entries += struct.pack(
                "<BBBBHHII",
                w, w, 0, 0, 1, 32, len(data), offset,
            )
            offset += len(data)
        out += entries
        for _, data in images:
            out += data
        with open(dest, "wb") as f:
            f.write(out)
        print(f"wrote {dest} ({len(out)} bytes, {n} sizes, rgb {r},{g},{b})")
        return
    sizes = [(16, 220, 60, 60), (24, 220, 60, 60), (32, 60, 190, 90),
             (48, 60, 150, 220), (64, 60, 90, 200)]
    images = [(s, make_image(s, r, g, b)) for s, r, g, b in sizes]
    n = len(images)
    out = struct.pack("<HHH", 0, 1, n)
    offset = 6 + 16 * n
    entries = b""
    for s, data in images:
        w = s if s < 256 else 0
        out_w = w
        entries += struct.pack(
            "<BBBBHHII",
            out_w, w, 0, 0, 1, 32, len(data), offset,
        )
        offset += len(data)
    out += entries
    for _, data in images:
        out += data
    dest = sys.argv[1] if len(sys.argv) > 1 else "test.ico"
    with open(dest, "wb") as f:
        f.write(out)
    print(f"wrote {dest} ({len(out)} bytes, {n} sizes)")


if __name__ == "__main__":
    main()
