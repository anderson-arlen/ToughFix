# Camera illustrations

## Olympus Tough TG-1

`tg-1.png` is an original generated illustration, embedded in the Rust executable.
It is not a device photograph or a separate runtime download. It was generated
with the built-in imagegen tool using the [Olympus product-history image](https://www.olympus-global.com/technology/museum/camera/products/digital-tough/tg-1/?page=technology_museum)
as an identity reference. The reference image is not distributed here.

Generation prompt:

> Use case: product-mockup. Asset type: compact camera illustration for a Linux
> desktop application's camera summary. Use the supplied Olympus Tough TG-1
> product photo as an identity reference. Generate a polished, original 3D product
> illustration of this silver-and-black rugged compact camera, front view with a
> very slight three-quarter angle, fully visible and centered, on a genuinely
> transparent background. Preserve its recognizable proportions: rounded
> rectangular silver front, black protective edges, textured black left grip
> with vertical OLYMPUS label, large central round dark lens, small flash upper
> right, restrained red Tough label on right. Clean brushed metal, soft natural
> highlights, readable at a small UI size. Keep the object alone, tight framing
> with a small transparent margin, no scene, no floor, no extra accessories, no
> captions, no watermark. Avoid adding tiny technical spec text.

## Olympus Stylus Tough-8010

`tough-8010.png` is an original generated, straight-on illustration of the
blue-accent model, embedded in the Rust
executable. It was generated with the built-in imagegen tool using the silver
front-view photo in [Olympus's Tough-8010 announcement](https://www.olympus.co.jp/jp/news/2010a/nr100202mjutough8010j.html)
as an identity reference and `tg-1.png` as a rendering-style reference. The
original product photographs are not distributed here. The
[blue-model product photo](https://www.ebay.com/p/108685314) was used only as a
color reference for the final edit.

Generation prompt:

> Use case: product-mockup. Asset type: compact camera illustration embedded in a Linux
> desktop application's camera summary. Input image 1 is an Olympus Tough-8010 official
> product photo, the identity reference. Input image 2 is the existing ToughFix TG-1
> illustration, used only for rendering style and framing. Generate a polished original
> 3D product illustration of the SILVER Olympus Stylus Tough-8010, matching image 1's
> exact recognizable body shape and controls. It is a flat silver rugged compact camera
> with a brushed metal front, a vertical silver OLYMPUS strip and two screws on the
> left, a small black square recessed lens in the UPPER RIGHT CORNER, a horizontal
> flash with a small adjacent light near the upper center, and a silver perimeter
> frame. Preserve the upper-right square lens; do not use the TG-1's large central
> round lens or black left grip. Show the entire camera from the front at a very slight
> three-quarter angle, fully visible and centered, tight framing with a small
> transparent margin, similar metallic materials and soft highlights to image 2. This
> is the Stylus Tough-8010 regional model; use restrained subtle Tough branding rather
> than prominent Japanese mu branding. No added technical spec lettering, no captions,
> no floor, no cast shadow outside the camera, no scene, no accessories, no watermark.
> Background must be genuinely transparent and the camera must remain readable at 180
> by 112 logical pixels.

Front-view correction prompt:

> Use case: precise-object-edit. Asset type: compact camera illustration for a desktop
> application. Change only the viewpoint of this silver Olympus Stylus Tough-8010
> illustration to an EXACT DEAD-ON FRONT VIEW. The viewer is directly perpendicular to
> the center of the camera's front panel, with zero yaw, zero pitch and zero roll. The
> upper and lower body edges must be horizontal and parallel, left and right edges
> vertical and parallel; absolutely no three-quarter angle, no visible right or left
> side panels, no visible top surface, no perspective foreshortening or tilted frame.
> Keep the identifiable Tough-8010 features: silver brushed rectangular rugged body,
> small square black lens at the upper right, horizontal flash near the upper center,
> narrow vertical OLYMPUS silver strip with two screws on the left, subdued vertical
> Tough label, lower right front screw. Preserve the metallic illustration style,
> materials and subdued lighting. Center the full camera with a small transparent
> margin. Maintain a genuinely transparent background, no floor, no scene, no detached
> shadow, no extra objects or captions. The result should look like a straight-on
> catalog elevation of the camera, readable at 180 by 112 UI pixels.

Blue-accent edit prompt:

> Use case: precise-object-edit. Asset type: camera illustration for the ToughFix
> desktop app. Image 1 is the edit target: a dead-on front elevation of an Olympus
> Stylus Tough-8010. Image 2 is ONLY a color and marking reference for the real
> BLUE-ACCENT Tough-8010. Change the large main front faceplate of image 1 from silver
> to metallic ocean blue, matching image 2. Blue occupies the broad panel to the RIGHT
> of the narrow vertical OLYMPUS strip, from the flash's lower edge down to the bottom
> inner seam and wrapping around the square lens cutout. Keep the outside perimeter
> frame, left OLYMPUS strip, screws, top strip, flash surround and lens surround
> silver; keep the black lens housing black. The vertical Tough branding on the blue
> faceplate should become subtle white, consistent with the reference. Preserve image
> 1's EXACT DEAD-ON FRONT VIEW, zero perspective, horizontal body edges, vertical left
> and right edges, no visible side panels or top surface. Preserve all features,
> proportions, materials, soft lighting, framing and genuinely transparent background
> from image 1. No added scene, accessories, captions, shadow or technical spec text.
> Do not introduce the angled viewpoint of image 2. The output is the same flat
> front-view illustration in the blue-accent colorway.

The illustrations are included under the project's MIT license. Olympus and Tough
names and product design identify the supported camera.
