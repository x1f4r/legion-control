#!/usr/bin/env bash
#
# Generates Resources/AppIcon.icns with the macOS system toolchain.
#
# The CoreGraphics drawing in this script is authoritative for raster output. Resources/icon.svg is
# the hand-authored master and uses the same geometry, colors, and gradients. Keeping the renderer
# here avoids depending on a third-party SVG rasterizer.
#
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SVG="$HERE/Resources/icon.svg"
ICONSET="$HERE/Resources/AppIcon.iconset"
ICNS="$HERE/Resources/AppIcon.icns"

if [[ ! -f "$SVG" ]]; then
	echo "Missing master artwork: $SVG" >&2
	exit 1
fi

TEMP_ROOT="$(mktemp -d "$HERE/.icon-build.XXXXXX")"
trap 'rm -rf "$TEMP_ROOT"' EXIT
STAGE_ICONSET="$TEMP_ROOT/AppIcon.iconset"
STAGE_ICNS="$TEMP_ROOT/AppIcon.icns"
mkdir -p "$STAGE_ICONSET"

xcrun swift - "$STAGE_ICONSET" <<'SWIFT'
import AppKit
import CoreGraphics
import Foundation

let canvas: CGFloat = 1024

func color(_ hex: UInt32, alpha: CGFloat = 1) -> CGColor {
    let red = CGFloat((hex >> 16) & 0xff) / 255
    let green = CGFloat((hex >> 8) & 0xff) / 255
    let blue = CGFloat(hex & 0xff) / 255
    return CGColor(red: red, green: green, blue: blue, alpha: alpha)
}

func squirclePath() -> CGPath {
    let path = CGMutablePath()
    path.move(to: CGPoint(x: 512, y: 48))
    path.addCurve(to: CGPoint(x: 920, y: 120), control1: CGPoint(x: 768, y: 48), control2: CGPoint(x: 848, y: 48))
    path.addCurve(to: CGPoint(x: 976, y: 512), control1: CGPoint(x: 976, y: 192), control2: CGPoint(x: 976, y: 272))
    path.addCurve(to: CGPoint(x: 920, y: 904), control1: CGPoint(x: 976, y: 752), control2: CGPoint(x: 976, y: 832))
    path.addCurve(to: CGPoint(x: 512, y: 976), control1: CGPoint(x: 848, y: 976), control2: CGPoint(x: 768, y: 976))
    path.addCurve(to: CGPoint(x: 104, y: 904), control1: CGPoint(x: 256, y: 976), control2: CGPoint(x: 176, y: 976))
    path.addCurve(to: CGPoint(x: 48, y: 512), control1: CGPoint(x: 48, y: 832), control2: CGPoint(x: 48, y: 752))
    path.addCurve(to: CGPoint(x: 104, y: 120), control1: CGPoint(x: 48, y: 272), control2: CGPoint(x: 48, y: 192))
    path.addCurve(to: CGPoint(x: 512, y: 48), control1: CGPoint(x: 176, y: 48), control2: CGPoint(x: 256, y: 48))
    path.closeSubpath()
    return path
}

func linearGradient(
    in context: CGContext,
    path: CGPath,
    colors: [CGColor],
    locations: [CGFloat],
    start: CGPoint,
    end: CGPoint
) {
    guard let gradient = CGGradient(
        colorsSpace: CGColorSpaceCreateDeviceRGB(),
        colors: colors as CFArray,
        locations: locations
    ) else {
        fatalError("Could not create a gradient")
    }
    context.saveGState()
    context.addPath(path)
    context.clip()
    context.drawLinearGradient(gradient, start: start, end: end, options: [.drawsBeforeStartLocation, .drawsAfterEndLocation])
    context.restoreGState()
}

func radialGradient(
    in context: CGContext,
    path: CGPath,
    colors: [CGColor],
    locations: [CGFloat],
    center: CGPoint,
    radius: CGFloat
) {
    guard let gradient = CGGradient(
        colorsSpace: CGColorSpaceCreateDeviceRGB(),
        colors: colors as CFArray,
        locations: locations
    ) else {
        fatalError("Could not create a gradient")
    }
    context.saveGState()
    context.addPath(path)
    context.clip()
    context.drawRadialGradient(
        gradient,
        startCenter: center,
        startRadius: 0,
        endCenter: center,
        endRadius: radius,
        options: [.drawsAfterEndLocation]
    )
    context.restoreGState()
}

func basePath() -> CGPath {
    let path = CGMutablePath()
    path.move(to: CGPoint(x: 92, y: 704))
    path.addLine(to: CGPoint(x: 932, y: 704))
    path.addCurve(to: CGPoint(x: 937, y: 722), control1: CGPoint(x: 940, y: 704), control2: CGPoint(x: 944, y: 714))
    path.addLine(to: CGPoint(x: 849, y: 788))
    path.addCurve(to: CGPoint(x: 815, y: 798), control1: CGPoint(x: 840, y: 795), control2: CGPoint(x: 829, y: 798))
    path.addLine(to: CGPoint(x: 209, y: 798))
    path.addCurve(to: CGPoint(x: 175, y: 788), control1: CGPoint(x: 195, y: 798), control2: CGPoint(x: 184, y: 795))
    path.addLine(to: CGPoint(x: 87, y: 722))
    path.addCurve(to: CGPoint(x: 92, y: 704), control1: CGPoint(x: 80, y: 714), control2: CGPoint(x: 84, y: 704))
    path.closeSubpath()
    return path
}

func drawIcon(in context: CGContext, pixels: Int) {
    let scale = CGFloat(pixels) / canvas
    context.setAllowsAntialiasing(true)
    context.setShouldAntialias(true)
    context.interpolationQuality = .high
    context.scaleBy(x: scale, y: scale)
    context.translateBy(x: 0, y: canvas)
    context.scaleBy(x: 1, y: -1)

    let icon = squirclePath()
    context.saveGState()
    context.setShadow(offset: CGSize(width: 0, height: 20), blur: 28, color: color(0x02050B, alpha: 0.42))
    context.addPath(icon)
    context.setFillColor(color(0x07101E))
    context.fillPath()
    context.restoreGState()

    context.saveGState()
    context.addPath(icon)
    context.clip()
    linearGradient(
        in: context,
        path: icon,
        colors: [color(0x26334B), color(0x111B2E), color(0x07101E)],
        locations: [0, 0.48, 1],
        start: CGPoint(x: 150, y: 92),
        end: CGPoint(x: 858, y: 954)
    )
    radialGradient(
        in: context,
        path: icon,
        colors: [color(0x5C789E, alpha: 0.28), color(0x304969, alpha: 0.07), color(0x0B1423, alpha: 0)],
        locations: [0, 0.62, 1],
        center: CGPoint(x: 316, y: 232),
        radius: 640
    )
    context.addPath(icon)
    context.setStrokeColor(color(0xA6C1E0, alpha: 0.19))
    context.setLineWidth(8)
    context.strokePath()

    let frame = CGPath(roundedRect: CGRect(x: 124, y: 214, width: 776, height: 522), cornerWidth: 64, cornerHeight: 64, transform: nil)
    context.saveGState()
    context.setShadow(offset: CGSize(width: 0, height: 22), blur: 34, color: color(0x02050B, alpha: 0.56))
    context.addPath(frame)
    context.setFillColor(color(0xD2DDE4))
    context.fillPath()
    context.restoreGState()
    linearGradient(
        in: context,
        path: frame,
        colors: [color(0xF2F6F8), color(0xD2DDE4), color(0x93A4B2)],
        locations: [0, 0.5, 1],
        start: CGPoint(x: 230, y: 214),
        end: CGPoint(x: 816, y: 744)
    )
    let frameHighlight = CGPath(roundedRect: CGRect(x: 127, y: 217, width: 770, height: 516), cornerWidth: 61, cornerHeight: 61, transform: nil)
    context.addPath(frameHighlight)
    context.setStrokeColor(color(0xFFFFFF, alpha: 0.34))
    context.setLineWidth(6)
    context.strokePath()

    let screen = CGPath(roundedRect: CGRect(x: 168, y: 258, width: 688, height: 402), cornerWidth: 30, cornerHeight: 30, transform: nil)
    context.saveGState()
    context.addPath(screen)
    context.clip()
    context.setFillColor(color(0x09111E))
    context.fill(CGRect(x: 168, y: 258, width: 688, height: 402))
    linearGradient(
        in: context,
        path: CGPath(rect: CGRect(x: 168, y: 258, width: 332, height: 402), transform: nil),
        colors: [color(0x58C2E1), color(0x347DC5)],
        locations: [0, 1],
        start: CGPoint(x: 174, y: 258),
        end: CGPoint(x: 486, y: 660)
    )
    linearGradient(
        in: context,
        path: CGPath(rect: CGRect(x: 524, y: 258, width: 332, height: 402), transform: nil),
        colors: [color(0xEEAC59), color(0xC75F4F)],
        locations: [0, 1],
        start: CGPoint(x: 538, y: 258),
        end: CGPoint(x: 850, y: 660)
    )
    linearGradient(
        in: context,
        path: screen,
        colors: [color(0xFFFFFF, alpha: 0.14), color(0xFFFFFF, alpha: 0), color(0x07101E, alpha: 0.12)],
        locations: [0, 0.48, 1],
        start: CGPoint(x: 512, y: 258),
        end: CGPoint(x: 512, y: 660)
    )
    context.restoreGState()
    context.addPath(screen)
    context.setStrokeColor(color(0x06101C, alpha: 0.74))
    context.setLineWidth(8)
    context.strokePath()

    let base = basePath()
    linearGradient(
        in: context,
        path: base,
        colors: [color(0xF5F7F8), color(0xCAD5DC), color(0x8E9DAA)],
        locations: [0, 0.42, 1],
        start: CGPoint(x: 512, y: 704),
        end: CGPoint(x: 512, y: 792)
    )
    context.move(to: CGPoint(x: 94, y: 708))
    context.addLine(to: CGPoint(x: 930, y: 708))
    context.setStrokeColor(color(0xFFFFFF, alpha: 0.58))
    context.setLineWidth(7)
    context.setLineCap(.round)
    context.strokePath()
    context.move(to: CGPoint(x: 176, y: 788))
    context.addCurve(to: CGPoint(x: 210, y: 798), control1: CGPoint(x: 185, y: 795), control2: CGPoint(x: 196, y: 798))
    context.addLine(to: CGPoint(x: 814, y: 798))
    context.addCurve(to: CGPoint(x: 848, y: 788), control1: CGPoint(x: 828, y: 798), control2: CGPoint(x: 839, y: 795))
    context.setStrokeColor(color(0x657482, alpha: 0.58))
    context.setLineWidth(5)
    context.strokePath()
    context.restoreGState()
}

func render(pixels: Int, to url: URL) throws {
    guard let context = CGContext(
        data: nil,
        width: pixels,
        height: pixels,
        bitsPerComponent: 8,
        bytesPerRow: 0,
        space: CGColorSpaceCreateDeviceRGB(),
        bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
    ) else {
        throw NSError(domain: "IconRenderer", code: 1, userInfo: [NSLocalizedDescriptionKey: "Could not create a bitmap context"])
    }
    drawIcon(in: context, pixels: pixels)
    guard let image = context.makeImage() else {
        throw NSError(domain: "IconRenderer", code: 2, userInfo: [NSLocalizedDescriptionKey: "Could not create a bitmap image"])
    }
    let representation = NSBitmapImageRep(cgImage: image)
    guard let data = representation.representation(using: .png, properties: [:]) else {
        throw NSError(domain: "IconRenderer", code: 3, userInfo: [NSLocalizedDescriptionKey: "Could not encode PNG data"])
    }
    try data.write(to: url, options: .atomic)
}

let outputs: [(String, Int)] = [
    ("icon_16x16.png", 16),
    ("icon_16x16@2x.png", 32),
    ("icon_32x32.png", 32),
    ("icon_32x32@2x.png", 64),
    ("icon_128x128.png", 128),
    ("icon_128x128@2x.png", 256),
    ("icon_256x256.png", 256),
    ("icon_256x256@2x.png", 512),
    ("icon_512x512.png", 512),
    ("icon_512x512@2x.png", 1024),
]

guard CommandLine.arguments.count == 2 else {
    fputs("Usage: IconRenderer OUTPUT_DIRECTORY\n", stderr)
    exit(2)
}

let outputDirectory = URL(fileURLWithPath: CommandLine.arguments[1], isDirectory: true)
do {
    for (name, pixels) in outputs {
        try render(pixels: pixels, to: outputDirectory.appendingPathComponent(name))
    }
} catch {
    fputs("Icon rendering failed: \(error.localizedDescription)\n", stderr)
    exit(1)
}
SWIFT

iconutil -c icns "$STAGE_ICONSET" -o "$STAGE_ICNS"
[[ -s "$STAGE_ICNS" ]] || { echo "iconutil did not create an icon" >&2; exit 1; }

rm -rf "$ICONSET"
mv "$STAGE_ICONSET" "$ICONSET"
install -m 0644 "$STAGE_ICNS" "$ICNS"

echo "Generated $ICNS"
