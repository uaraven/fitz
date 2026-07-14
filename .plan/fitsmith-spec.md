# FitSmith 

FitSmith is a GUI application aimed at astrophotographers. It allows quick operations on FITS files:
  - debayering
  - simple stretch
  - preview
    - raw undebayered image
    - debayered
  - blink
  - view FITS headers
  - view image stats
  - compress/decompress

FitSmith is designed to operate on lists of files while allowing select any of the files for the preview, operations

FitSmith uses Qt6 library for the UI using cxx-qt.

It is basically a GUI version of fitz-cli with some additional features.


FitSmith is a cross-platform application and is expected to work on Linux, MacOS and Windows.

## UI layout

The screen is divided into 2 panels - left side contains the list of files, the right side contains two tabs - one displays the image, the other displayes the FITS headers.

The image should allow zooming in and out, fitting the image to the view size or displaying it in 1:1 pixel scale.

There is a tool bar at the top of the screen. It contains checkbuttons to enable debayering and stretching of the image previews and toggling "show image stats" option.

If image stats is selected it is shown in a panel at the bottom of the image. Image view shrinks to accomodate the stats panel


There is a main menu with the same options as tool bar 

Menu structure:
 - File
   - Open... 
   - Open directory...
   - Save...
   - Save As...
   - -----
   - Exit 
 - Edit
   - Compress
   - Decompress
   - Copy header
   - Paste header
 - View
   - Debayer raw files ☑
   - Stretch preview ☑
   - Show image stats ☑
 - Help
   - About

More options will be added later

##
