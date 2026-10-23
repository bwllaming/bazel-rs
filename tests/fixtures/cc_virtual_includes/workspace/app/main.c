#include "app/consumer.h"
#include "stripped/api.h"

int main(void) { return consumer_value() == 18 && stripped_value() == 3 ? 0 : 1; }
