// Draws Scopewerk's app icon into an .iconset folder (all sizes iconutil expects).
// Usage: swift scripts/make-icon.swift <out.iconset>
//
// A small instrument: dark bezel, a scope screen with a dotted graticule, a yellow sine (CH1)
// and a cyan square wave (CH2), the DOS1102's channel colours.

import AppKit
import CoreGraphics
import Foundation

func draw(size: CGFloat) -> CGImage? {
    let pixels = Int(size)
    guard let context = CGContext(
        data: nil, width: pixels, height: pixels, bitsPerComponent: 8, bytesPerRow: 0,
        space: CGColorSpace(name: CGColorSpace.sRGB)!,
        bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
    ) else { return nil }
    let s = size / 1024
    func rect(_ x: CGFloat, _ y: CGFloat, _ w: CGFloat, _ h: CGFloat) -> CGRect {
        CGRect(x: x * s, y: y * s, width: w * s, height: h * s)
    }
    func color(_ hex: UInt32, _ alpha: CGFloat = 1) -> CGColor {
        CGColor(
            srgbRed: CGFloat((hex >> 16) & 0xff) / 255, green: CGFloat((hex >> 8) & 0xff) / 255,
            blue: CGFloat(hex & 0xff) / 255, alpha: alpha)
    }

    // Body squircle (macOS icon grid: 824 px body with 100 px margins).
    let body = rect(100, 100, 824, 824)
    let bodyPath = CGPath(roundedRect: body, cornerWidth: 185 * s, cornerHeight: 185 * s, transform: nil)
    context.saveGState()
    context.setShadow(offset: CGSize(width: 0, height: -10 * s), blur: 28 * s, color: color(0x000000, 0.3))
    context.addPath(bodyPath)
    context.setFillColor(color(0x1c222b))
    context.fillPath()
    context.restoreGState()
    context.saveGState()
    context.addPath(bodyPath)
    context.clip()
    let gradient = CGGradient(
        colorsSpace: CGColorSpace(name: CGColorSpace.sRGB), colors: [color(0x2a313c), color(0x12161c)] as CFArray,
        locations: [0, 1])!
    context.drawLinearGradient(gradient, start: CGPoint(x: 0, y: 924 * s), end: CGPoint(x: 0, y: 100 * s), options: [])
    context.restoreGState()

    // The screen.
    let screen = rect(178, 262, 668, 500)
    context.addPath(CGPath(roundedRect: screen, cornerWidth: 44 * s, cornerHeight: 44 * s, transform: nil))
    context.setFillColor(color(0x06080b))
    context.fillPath()
    context.saveGState()
    context.addPath(CGPath(roundedRect: screen, cornerWidth: 44 * s, cornerHeight: 44 * s, transform: nil))
    context.clip()

    // Dotted graticule: 8 x 6 divisions.
    let left: CGFloat = 178, bottom: CGFloat = 262, width: CGFloat = 668, height: CGFloat = 500
    context.setFillColor(color(0xffffff, 0.22))
    for column in 1..<8 {
        let x = left + width * CGFloat(column) / 8
        var y = bottom + 10
        while y < bottom + height {
            context.fill(rect(x - 3, y, 6, 6))
            y += 25
        }
    }
    for row in 1..<6 {
        let y = bottom + height * CGFloat(row) / 6
        var x = left + 10
        while x < left + width {
            context.fill(rect(x, y - 3, 6, 6))
            x += 25
        }
    }

    func trace(_ points: [CGPoint], _ hex: UInt32) {
        let path = CGMutablePath()
        path.addLines(between: points.map { CGPoint(x: $0.x * s, y: $0.y * s) })
        context.addPath(path)
        context.setLineWidth(30 * s)
        context.setLineCap(.round)
        context.setLineJoin(.round)
        context.setStrokeColor(color(hex, 0.25))
        context.strokePath()
        context.addPath(path)
        context.setLineWidth(14 * s)
        context.setStrokeColor(color(hex))
        context.strokePath()
    }

    // CH2: square wave in the lower half.
    let low: CGFloat = 340, high: CGFloat = 450
    var square: [CGPoint] = [CGPoint(x: 150, y: low)]
    var x: CGFloat = 230
    var up = true
    while x < 900 {
        let last = square[square.count - 1]
        square.append(CGPoint(x: x, y: last.y))
        square.append(CGPoint(x: x, y: up ? high : low))
        up.toggle()
        x += 140
    }
    square.append(CGPoint(x: 900, y: square[square.count - 1].y))
    trace(square, 0x2ec8f0)

    // CH1: sine in the upper half.
    let sine = stride(from: CGFloat(150), through: 900, by: 6).map { x in
        CGPoint(x: x, y: 600 + 92 * sin((x - 178) / 668 * 2 * .pi * 1.5))
    }
    trace(sine, 0xf5d90a)
    context.restoreGState()

    // Three knobs under the screen.
    for (index, hex) in [0xf5d90a, 0x2ec8f0, 0xff9f1c].enumerated() {
        let cx = 372 + CGFloat(index) * 140
        context.addEllipse(in: rect(cx - 34, 150, 68, 68))
        context.setFillColor(color(0x0d1014))
        context.fillPath()
        context.addEllipse(in: rect(cx - 22, 162, 44, 44))
        context.setFillColor(color(UInt32(hex)))
        context.fillPath()
    }
    return context.makeImage()
}

func writePNG(_ image: CGImage, to url: URL) throws {
    let rep = NSBitmapImageRep(cgImage: image)
    guard let data = rep.representation(using: .png, properties: [:]) else {
        throw NSError(domain: "icon", code: 1)
    }
    try data.write(to: url)
}

let arguments = CommandLine.arguments
guard arguments.count == 2 else {
    FileHandle.standardError.write("usage: make-icon.swift <out.iconset>\n".data(using: .utf8)!)
    exit(2)
}
let folder = URL(fileURLWithPath: arguments[1])
try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
for base in [16, 32, 128, 256, 512] {
    for scale in [1, 2] {
        let pixels = CGFloat(base * scale)
        let name = scale == 1 ? "icon_\(base)x\(base).png" : "icon_\(base)x\(base)@2x.png"
        guard let image = draw(size: pixels) else { exit(1) }
        try writePNG(image, to: folder.appendingPathComponent(name))
    }
}
