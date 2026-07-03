# TODO for release

 - [x] Ask before overwriting file (rename `-f` option to `-y`)
 - [ ] Test rice compression with float data (shouldn't work)
 - [ ] Add low level tools:
   - [x] Show all headers
   - [x] Show pixel data and histogram
   - [ ] Extract image data to file as a blob
   - [ ] convert between bitpix formats

**Stretch goals**
 - [ ] Add hcompress compression with float quantization


## Bugs

 - [ ] preview: Stretching already stretched images produces bad results ![](bad-stretch.png). Two options:
   - assume non-linear. stretch only if `--stretch` is passed
   - assume linear, don't stretch if `--no-stretch` passed
