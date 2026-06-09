//go:build embed_assets

package main

import "embed"

//go:embed deploy scripts
var embeddedAssets embed.FS
