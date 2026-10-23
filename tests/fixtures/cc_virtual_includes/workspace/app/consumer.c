#include "app/consumer.h"

#include "ext/ext.h"
#include "pre/both/both.h"
#include "stripped/api.h"
#include "vendor/prefixed/prefixed.h"

int consumer_value(void) {
  return stripped_value() + prefixed_value() + both_value() + ext_value();
}
