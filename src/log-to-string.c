#include <stdio.h>
#include <stdarg.h>
#include <stdlib.h>

char *log_to_string(const char *fmt, va_list vargs) {
    char *buffer;
    int size = 256;

    // Attempt to allocate memory for the buffer
    if ((buffer = malloc(size)) == NULL) return NULL;

    while (1) {
        va_list vargs_copy;
        va_copy(vargs_copy, vargs);
        int n = vsnprintf(buffer, size, fmt, vargs_copy);
        va_end(vargs_copy);

        if (n > -1 && n < size) { // Successfully formatted
            break;
        }

        if (n > -1)    // ISO/IEC 9899:1999
            size = n + 1;
        else           // Twice the old size
            size *= 2;

        char *new_buffer = realloc(buffer, size);
        if (new_buffer == NULL) {
            free(buffer);
            return NULL;
        }
        buffer = new_buffer;
    }

    return buffer;
}

void log_to_string_free(char *buffer) {
    free(buffer);
}