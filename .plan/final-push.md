# Specs for v 0.2.0

## Aberration inspector

Aberration inspector view similar to Siril's or ASTAP's.

Aberration inspector opens a dialog which displays mosaic of 9 squares. Each square size SZ is min(10% of selected image, 256)px.

Corner squares show the corner SZ x SZ parts of the selected image, central square shows the center of the image and the rest shows central areas along the sides, respectively.

The dialog only has one button - close.
Dialog can be invoked via Tools -> Aberration Inspector... menu.

## Support for RGB images for statistics and star analysis.

Two approaches: 

1. Calculate monochrome luminosity for each pixel from R,G and B values and calculate statistics based on that luminosity image 
2. Calculate statistics based on one channel of RGB image. Choises: Red, as astro photos often have a lot of red; or Green, as there are twice as much data in green channel than in any other


## Performance improvements

 - Cache calculated stats, so that there is no need to recalculate them unless the selection changes.

## SIMD

 - Does Rayon support SIMD? Investigate SIMD support for speeding up processing. 
