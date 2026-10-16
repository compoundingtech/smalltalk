// Regenerate assets/icon-dev.png from assets/icon.png on macOS:
// swift scripts/generate-dev-icon.swift assets/icon.png assets/icon-dev.png
// CoreGraphics/ImageIO/CoreText, Helvetica-Bold, amber bottom badge; no signing inputs.
import Foundation
import CoreGraphics
import ImageIO
import CoreText

let input = URL(fileURLWithPath: CommandLine.arguments[1])
let image = CGImageSourceCreateImageAtIndex(CGImageSourceCreateWithURL(input as CFURL, nil)!, 0, nil)!
let w = image.width, h = image.height
let context = CGContext(data: nil, width: w, height: h, bitsPerComponent: 8, bytesPerRow: 0, space: CGColorSpaceCreateDeviceRGB(), bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue)!
context.draw(image, in: CGRect(x: 0, y: 0, width: w, height: h))
context.setFillColor(red: 1, green: 0.7, blue: 0.33, alpha: 1)
context.fill(CGRect(x: 0, y: 0, width: Double(w), height: Double(h) * 0.27))
let font = CTFontCreateWithName("Helvetica-Bold" as CFString, Double(h) * 0.18, nil)
let text = NSAttributedString(string: "DEV", attributes: [NSAttributedString.Key(kCTFontAttributeName as String): font, NSAttributedString.Key(kCTForegroundColorAttributeName as String): CGColor(red: 0.12, green: 0.12, blue: 0.18, alpha: 1)])
let line = CTLineCreateWithAttributedString(text)
context.textPosition = CGPoint(x: (Double(w) - CTLineGetTypographicBounds(line, nil, nil, nil)) / 2, y: Double(h) * 0.07)
CTLineDraw(line, context)
let target = URL(fileURLWithPath: CommandLine.arguments[2])
if FileManager.default.fileExists(atPath: target.path) {
  try FileManager.default.setAttributes([.posixPermissions: 0o644], ofItemAtPath: target.path)
}
let output = CGImageDestinationCreateWithURL(target as CFURL, "public.png" as CFString, 1, nil)!
CGImageDestinationAddImage(output, context.makeImage()!, nil)
guard CGImageDestinationFinalize(output) else { fatalError("Cannot write development icon") }
try FileManager.default.setAttributes([.posixPermissions: 0o444], ofItemAtPath: target.path)
