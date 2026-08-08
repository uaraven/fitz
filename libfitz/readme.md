# libfitz

The library that handles the main operations for the fitz and fitsmith

libfitz functions:
 - reading fits files
   - automatically decompressing, if needed
 - saving fits files
   - compressing if needed
 - debayering
 - stretching
 - extracting R,G,B channels
   - from RGB file
   - from CFA file (averaging two green channels)
 - converting to DynamicImage
 - collecting frame stats:
   - max
   - min
   - mean
   - median
   - AAD
   - MAD
   - estimated bit depth
   - counts: max, min, zero, saturated
   - histogram
   - background noise level
 - detecting stars
 - calculating star stats:
   - HFR
   - FWHM
   - eccentricity
