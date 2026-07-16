# Fitsmith: analytics feature.


## Graphing metrics over time

Build graphing framework that will allow perform analysis of the metrics on multiple FITS files then draw the chart of the change of the metric over time.

For example: analyze change of mean ADU over the session.

The algorithm:
 - read each checked file
 - calculate stats
 - create tuple (time, mean ADU)
 - sort tuples by time
 - Draw the chart

There need to be a new component for displaying time-value charts supporting the following features:

 - Fit to screen
 - Scale (from fit to screen to 4x zoom)
 - Display chart with marks and lines
 - Show tooltip with X,Y values when hovering over the data point mark
 - Export chart as PNG
 - Export data as CSV

The chart component must support both light and dark themes.

The spike: there might be already a slint component implementing all of these (or most of these) functions - this needs to be investigated and then the decision should be made whether to use 3d party components vs developing my own. I lean towards my own for better control.

For phase 1 there should be following analytic charts available: min, max, median, mean ADU, number of max/min ADU pixels.

All time series analytic charts should be displayed as a dialog with a dropbox to chose which metric to display and buttons to export chart, data and close the dialog.