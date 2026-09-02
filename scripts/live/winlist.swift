import CoreGraphics
import Foundation
let opts = CGWindowListOption(arrayLiteral: .optionOnScreenOnly, .excludeDesktopElements)
let list = CGWindowListCopyWindowInfo(opts, kCGNullWindowID) as! [[String: Any]]
for w in list where (w["kCGWindowLayer"] as? Int) == 0 {
    let owner = w["kCGWindowOwnerName"] as? String ?? "?"
    let num = w["kCGWindowNumber"] as? Int ?? 0
    let name = w["kCGWindowName"] as? String ?? ""
    print("\(owner) #\(num) \(name)")
}
