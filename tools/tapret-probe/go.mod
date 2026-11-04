module tapretprobe

go 1.26.5

require github.com/arkade-os/arkd/pkg/ark-lib v0.0.0

replace github.com/arkade-os/arkd/pkg/ark-lib => ../../vendor/arkd/pkg/ark-lib

replace github.com/arkade-os/arkd/pkg/errors => ../../vendor/arkd/pkg/errors

replace github.com/btcsuite/btcd/btcec/v2 => github.com/btcsuite/btcd/btcec/v2 v2.3.3
